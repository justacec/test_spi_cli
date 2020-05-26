#![feature(proc_macro_hygiene, decl_macro)]
#[allow(dead_code)]
#[macro_use] extern crate serde_derive;
#[macro_use] extern crate json;


extern crate chrono;

#[allow(dead_code)]
mod util;
use crate::util::event::{Event, Events};

use std::fmt::Debug;
use std::vec::Vec;
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::JoinHandle;
use byte::ctx::*;
use byte::{BytesExt, TryWrite, LE};

use std::{error::Error, io};
use termion::{event::Key, input::MouseTerminal, raw::IntoRawMode, screen::AlternateScreen};
use tui::{
    backend::TermionBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Row, Table, TableState},
    Terminal,
};
use chrono::{DateTime, Local, Utc};
use bbqueue::{BBBuffer, ConstBBBuffer,
    framed::{FrameConsumer, FrameProducer, FrameGrantW, FrameGrantR}, 
    consts::*};
use once_cell::sync::Lazy;
use linux_embedded_hal::{
    spidev::{Spidev, SpidevOptions, SpiModeFlags},
    sysfs_gpio::{Pin, Direction, Edge}
};
use hex::encode;
use spmc;
use spmc::{Sender, Receiver};
use std::io::{Write, Read};
use rand::Rng;
use std::thread::sleep;
use std::time::Duration;

pub struct SPICommand {
    creation_time: DateTime<Utc>,
    response_time: Option<DateTime<Utc>>,
    id: u16,
    opcode: u16,
    data: Vec<u8>,
    response: Option<Vec<u8>>
}

impl SPICommand {
    fn new(id: u16, opcode: u16, data: Vec<u8>) -> SPICommand {
        SPICommand {
            creation_time: Utc::now(),
            response_time: None,
            id: id,
            opcode: opcode,
            data: data,
            response: None
        }
    }

    fn is_completed(&self) -> bool {
        self.response.is_some()
    }

    fn serialize(&self) -> Vec<u8> {
        let mut ret: Vec<u8> = vec![0u8; 4];

        ret.write_with::<u16>(&mut 0, self.id, LE);
        ret.write_with::<u16>(&mut 2, self.opcode, LE);
        ret.extend(self.data.iter().cloned());

        ret
    }

    fn get_row_vector(&self) -> Vec<String> {
        vec![
            self.creation_time.format("%H:%M:%S.%f").to_string(), 
            self.data.len().to_string(),
            self.id.to_string(),
            self.opcode.to_string(),
            hex::encode(&self.data).chars()
                .enumerate()
                .flat_map(|(i, c)| {
                    if i != 0 && i % 2 == 0 {
                        Some(' ')
                    } else {
                        None
                    }
                    .into_iter()
                    .chain(std::iter::once(c))
                })
                .collect::<String>(),
            "".to_string(),
            match self.response_time {
                Some(t) => {
                    t.format("%H:%M:%S.%f").to_string()
                },
                None => {
                    "".to_string()
                }
            },    
            match &self.response {
                Some(d) => {
                    hex::encode(&d).chars()
                        .enumerate()
                        .flat_map(|(i, c)| {
                            if i != 0 && i % 2 == 0 {
                                Some(' ')
                            } else {
                                None
                            }
                            .into_iter()
                            .chain(std::iter::once(c))
                        })
                        .collect::<String>()
                },
                None => {
                    "".to_string()
                }
            }    
        ]
    }
}

#[derive(PartialEq)]
pub enum SPIQueueState {
    Stopped,
    Running
}

pub struct SPIChannel {
    tx_queue_producer: Sender<Vec<u8>>,
    tx_queue_consumer: Receiver<Vec<u8>>,
    commands: Vec<SPICommand>,
    rx_state: Arc<Mutex<SPIQueueState>>,
    tx_state: Arc<Mutex<SPIQueueState>>,
    spidev: Arc<Mutex<Spidev>>,
    bsy: Arc<Pin>,
}

impl SPIChannel {
    fn new(filename: &str) -> SPIChannel {
        let mut spi = Spidev::open(filename).unwrap();
        
        let options = SpidevOptions::new()
         .bits_per_word(8)
         .max_speed_hz(18_000)
         .mode(SpiModeFlags::SPI_MODE_0)
         .build();
        
        spi.configure(&options).unwrap();
        
        let bsy_pin = Pin::new(23);
        bsy_pin.export().unwrap();
        bsy_pin.set_direction(linux_embedded_hal::sysfs_gpio::Direction::In).unwrap();  
        bsy_pin.set_edge(Edge::RisingEdge).unwrap();      

        let (tx_queue_producer, tx_queue_consumer): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = spmc::channel();

        SPIChannel {
            tx_queue_producer: tx_queue_producer,
            tx_queue_consumer: tx_queue_consumer,
            commands: Vec::new(),
            rx_state: Arc::new(Mutex::new(SPIQueueState::Stopped)),
            tx_state: Arc::new(Mutex::new(SPIQueueState::Stopped)),
            spidev: Arc::new(Mutex::new(spi)),
            bsy: Arc::new(bsy_pin)
        }
    }

    fn start_inturrupt(&self) {
        // Fire off the thread to watch for data returns
        let bsy_pin = self.bsy.clone();
        thread::spawn(move || {
            let mut poller = bsy_pin.get_poller().unwrap();
            loop {
                match poller.poll(1000).unwrap() {
                    Some(v) => {
                        let mut SPI_guard = SPI.lock().unwrap();
                        SPI_guard.process_rx();     
                    },
                    None => {}
                };
            }
        });

    }

    fn process_tx(&mut self) {

        let status = self.tx_state.clone();
        {
            let mut status_guard = self.tx_state.lock().unwrap();
            if *status_guard == SPIQueueState::Running {
                return;
            } else {
                *status_guard = SPIQueueState::Running;
            }
        }

        let a = self.tx_queue_consumer.clone();
        let spi = self.spidev.clone();
        let bsy = self.bsy.clone();

        thread::spawn(move || {
            loop {
                // Get a message
                let d = a.try_recv();
                match d {
                    Ok(msg_data) => {
                        // Send it on the SPI device

                        // This is a cheep hackinsh way to wait for the right time to send a message
                        loop {
                            if bsy.get_value().unwrap() == 0 {
                                break;
                            }
                            println!("Waiting!\n");
                            sleep(Duration::from_millis(10));
                        }

                        {
                            let mut spi_guard = spi.lock().unwrap();
                            spi_guard.write(&msg_data).unwrap();
                        }

                        // Put in a wait between messages to give MCU time to think
                        sleep(Duration::from_micros(100));
                    },
                    Err(_) => {
                        break;
                    }
                }
            }
            

            {
                let mut status_guard = status.lock().unwrap();
                *status_guard = SPIQueueState::Stopped;
            }
            return
        });
    }

    fn process_rx(&mut self) {
        let spi = self.spidev.clone();
        thread::spawn(move || {
            // Do a 4 byte read from the SPI port
            let mut incomming_raw= [0u8; 4];
            {
                let mut spi_guard = spi.lock().unwrap();
                spi_guard.read(&mut incomming_raw).unwrap();
            }

            let message_id: u16 = incomming_raw.read_with(&mut 0, LE).unwrap();
            let message_size: u16 = incomming_raw.read_with(&mut 2, LE).unwrap();

            // Create a receive buffer
            let mut rx_buf = vec![0_u8; message_size as usize];

            // Lock the SPI device and read the data
            {
                let mut spi_guard = spi.lock().unwrap();
                spi_guard.read(&mut rx_buf);
            }

            // Update the result table
            {
                let mut spi_guard = SPI.lock().unwrap();
                spi_guard.receive_response(message_id, rx_buf)
            }

            return
        });
    }

    fn send_command(&mut self, cmd: SPICommand) {
        // Add the command to the vector of commands
        self.commands.push(cmd);

        // Put the data in the tx queue
        let serialized = self.commands.last().unwrap().serialize();
        self.tx_queue_producer.send(serialized).unwrap();

        // Fire off the transmit queue
        self.process_tx();
    }

    fn receive_response(&mut self, id: u16, data: Vec<u8>) {
        match self.commands.iter().position(|r| r.id == id) {
            Some(index) => {
                self.commands[index].response_time = Some(Utc::now());
                self.commands[index].response = Some(data);        
            },
            None => {}
        }

    }
}

static SPI: Lazy<Mutex<SPIChannel>> = Lazy::new(|| {
    Mutex::new(SPIChannel::new("/dev/spidev1.1"))
});

pub struct StatefulTable {
    state: TableState,
    items: Vec<Vec<String>>,
}

impl StatefulTable {
    fn new() -> StatefulTable {
        StatefulTable {
            state: TableState::default(),
            items: vec![],
        }
    }
    pub fn next(&mut self) {
        let i = match self.state.selected() {
            Some(i) => {
                if i >= self.items.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }

    pub fn previous(&mut self) {
        let i = match self.state.selected() {
            Some(i) => {
                if i == 0 {
                    self.items.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }
}

#[allow(unreachable_code)]
fn main() -> Result<(), Box<dyn Error>> {
    let stdout = io::stdout().into_raw_mode()?;
    let backend = TermionBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let events = Events::new();

    let mut rng = rand::thread_rng();

    // Setup the data ready inturrupt
    {
        SPI.lock().unwrap().start_inturrupt();
    }

    let mut table = StatefulTable::new();
    let mut last_command_id = 0u16;

    loop {
        terminal.draw(|mut f| {
            let chunks = Layout::default()
            .direction(tui::layout::Direction::Vertical)
            .margin(1)
            .constraints(
                [
                    Constraint::Length(4),
                    Constraint::Min(10)
                ].as_ref()
            )
            .split(f.size());

            let block = Block::default()
                .title("Quick Commands")
                .borders(Borders::ALL);
            f.render_widget(block, chunks[0]);
            
            let incomplete_stye = Style::default().fg(Color::Red);
            let complete_style = Style::default().fg(Color::Green);

            let header = ["Tx Time", "Size", "ID", "Command", "Data", "", "Rx Time", "Response"];
            let rows: Vec<tui::widgets::Row<std::vec::IntoIter<std::string::String>>> = {
                let SPI_guard = SPI.lock().unwrap();
                SPI_guard.commands
                    .iter()
                    .map(|a| {
                        (a.is_completed(), a.get_row_vector())
                    })
                    .map(|i| {
                        if i.0 {
                            Row::StyledData(i.1.into_iter(), complete_style)
                        } else {
                            Row::StyledData(i.1.into_iter(), incomplete_stye)
                        }
                    })
                    .collect()
            };

            let t = Table::new(header.iter(), rows.into_iter())
                .block(Block::default().borders(Borders::ALL).title("Command Execution Timeline"))
                .widths(&[
                    Constraint::Length(20),
                    Constraint::Length(6),
                    Constraint::Length(6),
                    Constraint::Length(8),
                    Constraint::Min(30),
                    Constraint::Length(8),
                    Constraint::Length(20),
                    Constraint::Min(30)
                ]);
            f.render_stateful_widget(t, chunks[1], &mut table.state);
        }).unwrap();

        match events.next()? {
            Event::Input(key) => match key {
                Key::Char('q') => {
                    break;
                }
                Key::Char('r') => {
                    {
//                        let n = rng.gen_range(1, 10);
                        let n = 3;
                        for i in 0..n {
                            let length = rng.gen_range(0, 5);
                            let data = (0..length).map(|_| {
                                rng.gen_range(0, 255)
                            }).collect();
                            last_command_id += 1;
                            let cmd = SPICommand::new(last_command_id, 1, data);

                            {
                                let mut SPI_guard = SPI.lock().unwrap();
                                SPI_guard.send_command(cmd);            
                            }
                        }
                    }
                }
                Key::Char('s') => {

                    last_command_id += 1;
                    let cmd = SPICommand::new(last_command_id, 1, vec![0, 1, 2, 3, 4, 5]);

                    let mut SPI_guard = SPI.lock().unwrap();
                    SPI_guard.send_command(cmd);
                }
                Key::Down => {
                    table.next();
                }
                Key::Up => {
                    table.previous();
                }
                _ => {}
            },
            _ => {}
        };
        // Do event processing here somehow...
    };

    Ok(())
}
