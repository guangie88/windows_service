#![no_main]
#![feature(link_args)]

#[macro_use]
extern crate error_chain;
extern crate futures;
extern crate futures_cpupool;

#[macro_use]
extern crate log;
extern crate log4rs;

#[macro_use]
extern crate serde_derive;
extern crate subprocess;
extern crate toml;

#[macro_use]
extern crate winservice;

use futures::Future;
use futures_cpupool::CpuPool;
use log::LogLevelFilter;
use log4rs::append::file::FileAppender;
use log4rs::config::{Appender, Config, Root};
use log4rs::encode::pattern::PatternEncoder;
use std::env;
use std::fs::File;
use std::io::{self, Read};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use subprocess::{Exec, ExitStatus};

mod errors {
    error_chain! {
        errors {
        }
    }
}

use errors::*;

#[derive(Serialize, Deserialize, Debug)]
struct FileConfig {
    cmds: Vec<String>,
}

#[allow(non_snake_case)]
#[allow(unused_variables)]
#[no_mangle]
pub extern "system" fn WinMain(
    h_instance : *const c_void, h_prev_instance : *const c_void,
    lp_cmd_line : *const c_char, n_cmd_show : c_int) -> c_int
{
    // the name does not seem to matter
    // it can be renamed during sc create <servicename>
    Service!("windows_service", service_main)
}

fn run(args: Vec<String>, end: Receiver<()>) -> Result<()> {
    // set up the logging by using the same file name as 
    let exe_path = env::current_exe()
        .chain_err(|| "Unable to get current executable path")?;

    let exe_dir_path = match exe_path.parent() {
        Some(exe_dir_path) => exe_dir_path,
        None => bail!(format!("Unable to get parent directory of executable path: {:?}", exe_path)),
    };

    let exe_file_stem = match exe_path.file_stem() {
        Some(exe_file_stem) => exe_file_stem,
        None => bail!("Unable to get file stem of executable path: {:?}", exe_path),
    };

    let log_file_path = {
        let mut tmp_file_path = exe_dir_path.join(exe_file_stem);
        tmp_file_path.set_extension("log");
        tmp_file_path
    };

    let file_appender = FileAppender::builder()
        .encoder(Box::new(PatternEncoder::new("{h({d(%Y-%m-%d %H:%M:%S %Z)} [{l}] - {m}{n})}")))
        .build(log_file_path)
        .chain_err(|| "Unable to create file appender")?;

    let log_config = Config::builder()
        .appender(Appender::builder().build("file_appender", Box::new(file_appender)))
        .build(Root::builder().appender("file_appender").build(LogLevelFilter::Debug))
        .chain_err(|| "Unable to create log configuration")?;

    let _ = log4rs::init_config(log_config)
        .chain_err(|| "Unable to initialize from log configuration")?;

    // main thread checking + configuration
    if args.len() != 2 {
        bail!("Invalid usage, expected: service <config path>");
    }

    let config_path = &args[1];

    let config_str = {
        let mut config_file = File::open(config_path)
            .chain_err(|| format!("Unable to open config file path at {:?}", config_path))?;

        let mut s = String::new();

        config_file.read_to_string(&mut s)
            .map(|_| s)
            .chain_err(|| "Unable to read config file into string")?
    };

    let config: FileConfig = toml::from_str(&config_str)
        .chain_err(|| format!("Unable to parse config as required toml format: {}", config_str))?;

    let (txs, rxs): (Vec<_>, Vec<_>) = (0..config.cmds.len())
        .map(|_| mpsc::channel::<()>())
        .unzip();

    // maintain the loop to stop service in a separate thread
    let _ = thread::spawn(move || {
        loop {
            if let Ok(_) = end.try_recv() {
                for (idx, tx) in txs.into_iter().enumerate() {
                    match tx.send(()) {
                        Ok(_) => debug!("Sent into channel #{}", idx),
                        Err(e) => error!("Error sending into channel #{}: {}", idx, e),
                    }
                }

                debug!("Received service end message");
                break;
            }
        }
    });
    
    // starts launching of processes

    // set up the CPU pool
    // needs * 2 because of each subprocess requires another force stopper future,    
    let required_pool_count = config.cmds.len() * 2;
    let pool = CpuPool::new(required_pool_count);

    let fut_threads: Vec<_> = rxs.into_iter().enumerate()
        .zip(config.cmds.iter().cloned())
        .map(|((idx, rx), process)| {
            // rx receiving for forced stop
            let rx_fut = pool.spawn_fn(move || {
                match rx.recv() {
                    Ok(_) => debug!("Received from channel #{}", idx),
                    Err(e) => error!("Error receiving from channel #{}: {}", idx, e),
                }

                Ok(ExitStatus::Undetermined)
            });

            // subprocess launch
            let subprocess_fut = pool.spawn_fn(move || {
                let process_str = process.clone();

                let process_run = move || -> Result<ExitStatus> {
                    // process thread body
                    let exit_status = Exec::shell(process)
                        .join()
                        .chain_err(|| format!("Unable to join shell process"))?;

                    Ok(exit_status)
                };

                let process_res = process_run();

                match process_res {
                    Ok(ref exit_status) => info!("Shell terminated [{}], exit code: {:?}", process_str, exit_status),
                    Err(ref e) => error!("Shell error [{}]: {}", process_str, e),
                }

                process_res
            });

            thread::spawn(move || {
                let win_res = rx_fut.select(subprocess_fut)
                    .map(|(win_fut, _)| win_fut)
                    .wait();

                match win_res {
                    Ok(ref exit_status) => info!("Process #{} exit status: {:?}", idx, exit_status),
                    Err((ref e, _)) => error!("Process #{} error: {}", idx, e),
                }

                win_res
            })
        })
        // must collect first in order to force all the futures to be executed
        .collect();
    
    let combined_res: std::result::Result<Vec<_>, _> = fut_threads.into_iter()
        .map(|fut_thread| fut_thread.join())
        .collect();

    if let Err(e) = combined_res {
        error!("Error combining threads: {:?}", e);
    }

    Ok(())
}

#[allow(unused_variables)]
fn service_main(args: Vec<String>, end: Receiver<()>) -> u32 {
    match run(args, end) {
        Ok(_) => {
            info!("Program completed!");
            0
        },

        Err(ref e) => {
            let stderr = &mut io::stderr();
            error!("Error: {}", e);

            for e in e.iter().skip(1) {
                error!("- Caused by: {}", e);
            }

            1
        },
    }
}