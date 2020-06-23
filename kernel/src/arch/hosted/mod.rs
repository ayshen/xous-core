pub mod irq;
pub mod mem;
pub mod process;
pub mod syscall;

use std::env;
use std::io::Read;
use std::mem::size_of;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread::spawn;
use std::time::Duration;

use xous::{Result, SysCall, PID};

use crate::arch::process::ProcessHandle;
use crate::services::SystemServicesHandle;

pub type KernelArguments = Option<String>;

const DEFAULT_LISTEN_ADDRESS: &str = "localhost:9687";

/// Each client gets its own connection and its own thread, which is handled here.
fn handle_connection(mut conn: TcpStream, pid: PID, chn: Sender<(PID, SysCall)>) {
    loop {
        let mut pkt = [0usize; 8];
        let mut incoming_word = [0u8; size_of::<usize>()];
        conn.set_nonblocking(true).expect("couldn't enable nonblocking mode");
        for word in pkt.iter_mut() {
            loop {
                if let Err(e) = conn.read_exact(&mut incoming_word) {
                    if e.kind() != std::io::ErrorKind::WouldBlock {
                        println!(
                            "Client {} disconnected: {}. Shutting down virtual process.",
                            pid, e
                        );
                        let call = xous::SysCall::TerminateProcess;
                        chn.send((pid, call)).unwrap();
                        return;
                    }
                    continue;
                }
                break;
            }
            *word = usize::from_le_bytes(incoming_word);
        }
        let call = xous::SysCall::from_args(
            pkt[0], pkt[1], pkt[2], pkt[3], pkt[4], pkt[5], pkt[6], pkt[7],
        );
        match call {
            Err(e) => println!("Received invalid syscall: {:?}", e),
            Ok(call) => {
                // println!(
                //     "Received packet: {:08x} {} {} {} {} {} {} {}: {:?}",
                //     pkt[0], pkt[1], pkt[2], pkt[3], pkt[4], pkt[5], pkt[6], pkt[7], call
                // );
                chn.send((pid, call)).expect("couldn't make syscall");
            }
        }
    }
}

fn listen_thread(address: Option<String>, chn: Sender<(PID, SysCall)>, quit: Receiver<()>) {
    let listen_addr = address.unwrap_or_else(|| {
        env::var("XOUS_LISTEN_ADDR").unwrap_or_else(|_| DEFAULT_LISTEN_ADDRESS.to_owned())
    });
    println!("Starting Xous server on {}...", listen_addr);
    let listener = TcpListener::bind(listen_addr).unwrap_or_else(|e| {
        panic!("Unable to create server: {}", e);
    });

    let mut clients = vec![];

    // Use `listener` in a nonblocking setup so that we can exit when doing tests
    listener
        .set_nonblocking(true)
        .expect("couldn't set TcpListener to nonblocking");
    loop {
        match listener.accept() {
            Ok((conn, addr)) => {
                let thr_chn = chn.clone();

                let new_pid = {
                    let mut ss = SystemServicesHandle::get();
                    ss.spawn_process(process::ProcessInit::new(conn.try_clone().unwrap()), ())
                        .unwrap()
                };
                println!("New client connected from {} and assigned PID {}", addr, new_pid);
                let conn_copy = conn.try_clone().expect("couldn't duplicate connection");
                let jh = spawn(move || handle_connection(conn, new_pid, thr_chn));
                clients.push((jh, conn_copy));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                match quit.recv_timeout(Duration::from_millis(10)) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        continue;
                    }
                    x => {
                        for (jh, conn) in clients {
                            use std::net::Shutdown;
                            conn.shutdown(Shutdown::Both).expect("couldn't shutdown client");
                            jh.join().expect("couldn't join client thread");
                        }
                        return;
                    }
                }
            }
            Err(e) => {
                eprintln!("error accepting connections: {}", e);
                return;
            }
        }
    }
}

/// The idle function is run when there are no directly-runnable processes
/// that kmain can activate. In a hosted environment,this is the primary
/// thread that handles network communications, and this function never returns.
pub fn idle(args: &KernelArguments) -> bool {
    // Start listening.
    let (sender, receiver) = channel();
    let (term_sender, term_receiver) = channel();

    let server_addr = args.clone();
    let listen_thread_handle = spawn(move || listen_thread(server_addr, sender, term_receiver));

    while let Ok((pid, call)) = receiver.recv() {
        {
            let mut ss = SystemServicesHandle::get();
            ss.switch_to(pid, Some(1)).unwrap();
        }

        // If the call being made is to terminate the current process, we need to know
        // because we won't be able to send a response.
        let is_terminate = call == SysCall::TerminateProcess;
        let is_shutdown = call == SysCall::Shutdown;

        // Handle the syscall within the Xous kernel
        let response = crate::syscall::handle(pid, call).unwrap_or_else(Result::Error);

        // There's a response if it wasn't a blocked process and we're not terminating.
        // Send the response back to the target.
        if response != Result::BlockedProcess && !is_terminate && !is_shutdown{
            {
                let mut processes = ProcessHandle::get();
                let mut response_vec = Vec::new();
                for word in response.to_args().iter_mut() {
                    response_vec.extend_from_slice(&word.to_le_bytes());
                }
                processes.send(&response_vec).unwrap_or_else(|e| {
                    // If we're unable to send data to the process, assume it's dead and terminate it.
                    println!("Unable to send response to process: {:?} -- terminating", e);
                    crate::syscall::handle(pid, SysCall::TerminateProcess).ok();
                });
            }
            let mut ss = SystemServicesHandle::get();
            ss.switch_from(pid, 1, true).unwrap();
        }

        if is_shutdown {
            let mut processes = ProcessHandle::get();
            let mut response_vec = Vec::new();
            for word in Result::Ok.to_args().iter_mut() {
                response_vec.extend_from_slice(&word.to_le_bytes());
            }
            processes.send(&response_vec).unwrap_or_else(|e| {
                // If we're unable to send data to the process, assume it's dead and terminate it.
                println!("Unable to send response to process: {:?} -- terminating", e);
                crate::syscall::handle(pid, SysCall::TerminateProcess).ok();
            });
            term_sender.send(()).expect("couldn't send shutdown signal");
            break;
        }
    }

    eprintln!("Exiting Xous because the listen thread channel has closed. Waiting for thread to finish...");
    listen_thread_handle
        .join()
        .expect("error waiting for listen thread to return");

    eprintln!("Thank you for using Xous!");
    false
}
