//! Implements a text console over the UART that allows
//! a terminal to inspect and control userspace processes.
//!
//! Protocol
//! --------
//!
//! This module provides a simple text-based console to inspect and control
//! which processes are running. The console has five commands:
//!  - 'help' prints the available commands and arguments
//!  - 'status' prints the current system status
//!  - 'list' lists the current processes with their IDs and running state
//!  - 'stop n' stops the process with name n
//!  - 'start n' starts the stopped process with name n
//!  - 'fault n' forces the process with name n into a fault state
//!
//! Setup
//! -----
//!
//! You need a device that provides the `hil::uart::UART` trait. This code
//! connects a `ProcessConsole` directly up to USART0:
//!
//! ```rust
//! pub struct Capability;
//! unsafe impl capabilities::ProcessManagementCapability for Capability {}
//!
//! let pconsole = static_init!(
//!     ProcessConsole<usart::USART>,
//!     ProcessConsole::new(&usart::USART0,
//!                  115200,
//!                  &mut console::WRITE_BUF,
//!                  &mut console::READ_BUF,
//!                  &mut console::COMMAND_BUF,
//!                  kernel,
//!                  Capability);
//! hil::uart::UART::set_client(&usart::USART0, pconsole);
//!
//! pconsole.initialize();
//! pconsole.start();
//! ```
//!
//! Buffer use and output
//! ---------------------
//! `ProcessConsole` does not use its own write buffer for output:
//! it uses the debug!() buffer, so as not to repeat all of its buffering and
//! to maintain a correct ordering with debug!() calls. The write buffer of
//! `ProcessConsole` is used solely for echoing what someone types.
//!
//! Using ProcessConsole
//! --------------------
//!
//! With this capsule properly added to a board's `main.rs` and that kernel
//! loaded to the board, make sure there is a serial connection to the board.
//! Likely, this just means connecting a USB cable from a computer to the board.
//! Next, establish a serial console connection to the board. An easy way to do
//! this is to run:
//!
//!     $ tockloader listen
//!
//! With that console open, you can issue commands. For example, to see all of
//! the processes on the board, use `list`:
//!
//! ```text
//! $ tockloader listen
//! Using "/dev/cu.usbserial-c098e513000c - Hail IoT Module - TockOS"
//!
//! Listening for serial output.
//! ProcessConsole::start
//! Starting process console
//! Initialization complete. Entering main loop
//! Hello World!
//! list
//! PID    Name    Quanta  Syscalls  Dropped Callbacks    State
//! 00     blink        0       113                  0  Yielded
//! 01     c_hello      0         8                  0  Yielded
//! ```
//!
//! To get a general view of the system, use the status command:
//!
//! ```text
//! status
//! Total processes: 2
//! Active processes: 2
//! Timeslice expirations: 0
//! ```
//!
//! and you can control processes with the `start` and `stop` commands:
//!
//! ```text
//! stop blink
//! Process blink stopped
//! ```

use core::cell::Cell;
use core::cmp;
use core::str;
use core::fmt::{write, Arguments, Result, Write};
use kernel::capabilities::ProcessManagementCapability;
use kernel::common::cells::TakeCell;
use kernel::debug;
use kernel::hil::uart;
use kernel::introspection::KernelInfo;
use kernel::Kernel;
use kernel::ReturnCode;

use crate::console_mux;

// Since writes are character echoes, we do not need more than 4 bytes:
// the longest write is 3 bytes for a backspace (backspace, space, backspace).
pub static mut WRITE_BUF: [u8; 4] = [0; 4];
// Since reads are byte-by-byte, to properly echo what's typed,
// we can use a very small read buffer.
pub static mut READ_BUF: [u8; 4] = [0; 4];
// Commands can be up to 32 bytes long: since commands themselves are 4-5
// characters, limiting arguments to 25 bytes or so seems fine for now.
pub static mut COMMAND_BUF: [u8; 32] = [0; 32];

pub struct ProcessConsoleWriter {
    tx_buffer: TakeCell<'static, [u8]>,
    tx_len: Cell<usize>,
}

impl ProcessConsoleWriter {
    pub fn new(
        tx_buffer: &'static mut [u8],
    ) -> ProcessConsoleWriter {
        ProcessConsoleWriter {
            tx_buffer: TakeCell::new(tx_buffer),
            tx_len: Cell::new(0),
        }
    }

    fn get_tx_buffer(&self) -> Option<&'static mut [u8]> {
        self.tx_buffer.take()
    }

    fn set_tx_buffer(&self, buffer: &'static mut [u8]) {
        self.tx_buffer.replace(buffer);
    }

    fn get_and_reset_tx_len(&self) -> usize {
        let l = self.tx_len.get();
        self.tx_len.set(0);
        l
    }
}

pub struct ProcessConsole<'a, C: ProcessManagementCapability> {
    console_mux: &'a console_mux::Console<'a>,
    writer: TakeCell<'static, ProcessConsoleWriter>,
    // tx_in_progress: Cell<bool>,
    // tx_buffer: TakeCell<'static, [u8]>,
    // tx_len: Cell<usize>,
    rx_in_progress: Cell<bool>,
    rx_buffer: TakeCell<'static, [u8]>,
    command_buffer: TakeCell<'static, [u8]>,
    command_index: Cell<usize>,
    running: Cell<bool>,
    kernel: &'static Kernel,
    capability: C,
}

impl<'a, C: ProcessManagementCapability> ProcessConsole<'a, C> {
    pub fn new(
        console_mux: &'a console_mux::Console<'a>,
        writer: &'static mut ProcessConsoleWriter,
        // tx_buffer: &'static mut [u8],
        rx_buffer: &'static mut [u8],
        cmd_buffer: &'static mut [u8],
        kernel: &'static Kernel,
        capability: C,
    ) -> ProcessConsole<'a, C> {
        ProcessConsole {
            console_mux: console_mux,
            writer: TakeCell::new(writer),
            // tx_in_progress: Cell::new(false),
            // tx_buffer: TakeCell::new(tx_buffer),
            // tx_len: Cell::new(0),
            rx_in_progress: Cell::new(false),
            rx_buffer: TakeCell::new(rx_buffer),
            command_buffer: TakeCell::new(cmd_buffer),
            command_index: Cell::new(0),
            running: Cell::new(false),
            kernel: kernel,
            capability: capability,
        }
    }

    pub fn start(&self) -> ReturnCode {
        if self.running.get() == false {
            self.rx_buffer.take().map(|buffer| {
                self.rx_in_progress.set(true);
                self.console_mux.receive_message(buffer);
                self.running.set(true);
            });
        }
        ReturnCode::SUCCESS
    }

    fn write_string(&self, args: Arguments) {
        self.writer.map(|writer| {
            let _ = write(writer, args);
            let _ = writer.write_str("\r\n");
        });
    }

    fn send(&self) {
        self.writer.map(|writer| {
            writer.get_tx_buffer().map(|tx_buffer| {
                let tx_len = writer.get_and_reset_tx_len();
                self.console_mux.transmit_message(tx_buffer, tx_len, None);
            });
        });
    }

    // Process the command in the command buffer and clear the buffer.
    fn read_command(&self) {
        self.command_buffer.map(|command| {
            let mut terminator = 0;
            let len = command.len();
            for i in 0..len {
                if command[i] == 0 {
                    terminator = i;
                    break;
                }
            }
            //debug!("Command: {}-{} {:?}", start, terminator, command);
            // A command is valid only if it starts inside the buffer,
            // ends before the beginning of the buffer, and ends after
            // it starts.
            if terminator > 0 {
                let cmd_str = str::from_utf8(&command[0..terminator]);
                match cmd_str {
                    Ok(s) => {
                        let clean_str = s.trim();
                        if clean_str.starts_with("help") {
                            self.write_string(format_args!("Welcome to the process console."));
                            self.write_string(format_args!("Valid commands are: help status list stop start"));
                        } else if clean_str.starts_with("start") {
                            let argument = clean_str.split_whitespace().nth(1);
                            argument.map(|name| {
                                self.kernel.process_each_capability(
                                    &self.capability,
                                    |_i, proc| {
                                        let proc_name = proc.get_process_name();
                                        if proc_name == name {
                                            proc.resume();
                                            self.write_string(format_args!("Process {} resumed.", name));
                                        }
                                    },
                                );
                            });
                        } else if clean_str.starts_with("stop") {
                            let argument = clean_str.split_whitespace().nth(1);
                            argument.map(|name| {
                                self.kernel.process_each_capability(
                                    &self.capability,
                                    |_i, proc| {
                                        let proc_name = proc.get_process_name();
                                        if proc_name == name {
                                            proc.stop();
                                            self.write_string(format_args!("Process {} stopped", proc_name));
                                        }
                                    },
                                );
                            });
                        } else if clean_str.starts_with("fault") {
                            let argument = clean_str.split_whitespace().nth(1);
                            argument.map(|name| {
                                self.kernel.process_each_capability(
                                    &self.capability,
                                    |_i, proc| {
                                        let proc_name = proc.get_process_name();
                                        if proc_name == name {
                                            proc.set_fault_state();
                                            self.write_string(format_args!("Process {} now faulted", proc_name));
                                        }
                                    },
                                );
                            });
                        } else if clean_str.starts_with("list") {
                            self.write_string(format_args!(" PID    Name                Quanta  Syscalls  Dropped Callbacks    State"));
                            self.kernel
                                .process_each_capability(&self.capability, |i, proc| {
                                    let pname = proc.get_process_name();
                                    self.write_string(format_args!(
                                        "  {:02}\t{:<20}{:6}{:10}{:19}  {:?}",
                                        i,
                                        pname,
                                        proc.debug_timeslice_expiration_count(),
                                        proc.debug_syscall_count(),
                                        proc.debug_dropped_callback_count(),
                                        proc.get_state()
                                    ));
                                });
                        } else if clean_str.starts_with("status") {
                            let info: KernelInfo = KernelInfo::new(self.kernel);
                            self.write_string(format_args!(
                                "Total processes: {}",
                                info.number_loaded_processes(&self.capability)
                            ));
                            self.write_string(format_args!(
                                "Active processes: {}",
                                info.number_active_processes(&self.capability)
                            ));
                            self.write_string(format_args!(
                                "Timeslice expirations: {}",
                                info.timeslice_expirations(&self.capability)
                            ));
                        } else {
                            self.write_string(format_args!("Valid commands are: help status list stop start fault"));
                        }
                    }
                    Err(_e) => {
                        self.write_string(format_args!("Invalid command: {:?}", command));
                    }
                }
                self.send();
            }
        });
        self.command_buffer.map(|command| {
            command[0] = 0;
        });
        self.command_index.set(0);
    }

    // fn write_byte(&self, byte: u8) -> ReturnCode {
    //     if self.tx_in_progress.get() {
    //         ReturnCode::EBUSY
    //     } else {
    //         self.tx_in_progress.set(true);
    //         self.tx_buffer.take().map(|buffer| {
    //             buffer[0] = byte;
    //             self.uart.transmit_buffer(buffer, 1);
    //         });
    //         ReturnCode::SUCCESS
    //     }
    // }

    // fn write_bytes(&self, bytes: &[u8]) -> ReturnCode {
    //     if self.tx_in_progress.get() {
    //         ReturnCode::EBUSY
    //     } else {
    //         self.tx_in_progress.set(true);
    //         self.tx_buffer.take().map(|buffer| {
    //             let len = cmp::min(bytes.len(), buffer.len());
    //             for i in 0..len {
    //                 buffer[i] = bytes[i];
    //             }
    //             self.uart.transmit_buffer(buffer, len);
    //         });
    //         ReturnCode::SUCCESS
    //     }
    // }
}

// impl<'a, C: ProcessManagementCapability> Write for ProcessConsole<'a, C> {
impl Write for ProcessConsoleWriter {
    fn write_str(&mut self, s: &str) -> Result {
        let start = self.tx_len.get();
        let end = start + s.len();

        self.tx_buffer.map(|tx_buffer| {
            for (dst, src) in tx_buffer[start..end].iter_mut().zip(s.as_bytes().iter()) {
                *dst = *src;
            }
        });

        self.tx_len.set(end);

        Ok(())
    }
}

impl<'a, C: ProcessManagementCapability> uart::TransmitClient for ProcessConsole<'a, C> {
    fn transmitted_buffer(&self, buffer: &'static mut [u8], _tx_len: usize, _rcode: ReturnCode) {
        self.writer.map(move |writer| {
            writer.set_tx_buffer(buffer);
        });


        // self.tx_in_progress.set(false);
    }
}
impl<'a, C: ProcessManagementCapability> uart::ReceiveClient for ProcessConsole<'a, C> {
    fn received_buffer(
        &self,
        read_buf: &'static mut [u8],
        rx_len: usize,
        _rcode: ReturnCode,
        error: uart::Error,
    ) {
        // let mut execute = false;
        if error == uart::Error::None {
        //     match rx_len {
        //         0 => debug!("ProcessConsole had read of 0 bytes"),
        //         1 => {
        //             self.command_buffer.map(|command| {
        //                 let index = self.command_index.get() as usize;
        //                 if read_buf[0] == ('\n' as u8) || read_buf[0] == ('\r' as u8) {
        //                     execute = true;
        //                     self.write_bytes(&['\r' as u8, '\n' as u8]);
        //                 } else if read_buf[0] == ('\x08' as u8) && index > 0 {
        //                     // Backspace, echo and remove last byte
        //                     // Note echo is '\b \b' to erase
        //                     self.write_bytes(&['\x08' as u8, ' ' as u8, '\x08' as u8]);
        //                     command[index - 1] = '\0' as u8;
        //                     self.command_index.set(index - 1);
        //                 } else if index < (command.len() - 1) && read_buf[0] < 128 {
        //                     // For some reason, sometimes reads return > 127 but no error,
        //                     // which causes utf-8 decoding failure, so check byte is < 128. -pal

        //                     // Echo the byte and store it
        //                     self.write_byte(read_buf[0]);
        //                     command[index] = read_buf[0];
        //                     self.command_index.set(index + 1);
        //                     command[index + 1] = 0;
        //                 }
        //             });
        //         }
        //         _ => debug!(
        //             "ProcessConsole issues reads of 1 byte, but receive_complete was length {}",
        //             rx_len
        //         ),
        //     };
        // }
        // self.rx_in_progress.set(true);
            self.console_mux.receive_message(read_buf);
        // if execute {
            self.read_command();
        }
    }
}
