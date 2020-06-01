#![feature(proc_macro_hygiene, decl_macro)]
#[allow(dead_code)]
#[macro_use] extern crate serde_derive;
#[macro_use] extern crate json;


extern crate chrono;

#[allow(dead_code)]
mod util;
use crate::util::event::{Event, Events};

use core::convert::TryFrom;
use std::fmt::Debug;
use std::vec::Vec;
use std::sync::{Arc, Mutex};
use std::thread;
use std::fmt;
use byte::ctx::*;
use byte::{BytesExt, LE};

use std::{error::Error, io};
use termion::{event::Key, raw::IntoRawMode};
use tui::{
    backend::TermionBackend,
    layout::{Constraint, Layout},
    style::{Color, Style},
    widgets::{Block, Borders, Row, Table, TableState, List, ListState, Text, Paragraph},
    Terminal,
    
};
use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use linux_embedded_hal::{
    spidev::{Spidev, SpidevOptions, SpiModeFlags},
    sysfs_gpio::{Pin, Edge}
};
use spmc;
use spmc::{Sender, Receiver};
use std::io::{Write, Read};
use rand::Rng;
use rand_distr::{Distribution, LogNormal};
use std::thread::sleep;
use time::Duration;

pub struct SPICommand {
    creation_time: DateTime<Utc>,
    response_time: Option<DateTime<Utc>>,
    id: u16,
    opcode: u16,
    data: Vec<u8>,
    response: Option<Vec<u8>>,
    delta_time: Option<chrono::Duration>,
    rowvec: Vec<String>
}

impl SPICommand {
    fn new(id: u16, opcode: u16, data: Vec<u8>) -> SPICommand {
        let mut tmp = SPICommand {
            creation_time: Utc::now(),
            response_time: None,
            id: id,
            opcode: opcode,
            data: data,
            response: None,
            delta_time: None,
            rowvec: Vec::new()
        };

        tmp.update_row_vector();

        tmp
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
        self.rowvec.clone()
    }

    fn update_row_vector(&mut self) {
        self.rowvec = vec![
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
            match self.delta_time {
                Some(d) => {
                    format!("{}", d)
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
        ];
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
    completed_commands: Vec<SPICommand>,
//    rx_state: Arc<Mutex<SPIQueueState>>,
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
            completed_commands: Vec::new(),
//            rx_state: Arc::new(Mutex::new(SPIQueueState::Stopped)),
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
                            {
                                let mut m = messages.lock().unwrap();
                                m.push(MessageLevel::INFO, "Waiting".to_string());
                            }
                            sleep(std::time::Duration::try_from(Duration::milliseconds(10)).unwrap());
                                //  from_millis(10));
                        }

                        {
                            let mut spi_guard = spi.lock().unwrap();
                            spi_guard.write(&msg_data).unwrap();
                        }

                        // Put in a wait between messages to give MCU time to think
                        sleep(std::time::Duration::try_from(Duration::microseconds(100)).unwrap());
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
            let mut a = Utc::now();
            {
                let mut spi_guard = spi.lock().unwrap();
                spi_guard.read(&mut incomming_raw).unwrap();
            }
            let mut b = Utc::now();
            let p1 = b-a;

            a = Utc::now();
            let message_id: u16 = incomming_raw.read_with(&mut 0, LE).unwrap();
            let message_size: u16 = incomming_raw.read_with(&mut 2, LE).unwrap();

            // Create a receive buffer
            let mut rx_buf = vec![0_u8; message_size as usize];
            b = Utc::now();
            let p2 = b-a;

            // Lock the SPI device and read the data
            a = Utc::now();
            {
                let mut spi_guard = spi.lock().unwrap();
                spi_guard.read(&mut rx_buf).unwrap();
            }
            b = Utc::now();
            let p3 = b-a;

            messages.lock().unwrap().push(MessageLevel::INFO, format!("{} : {} : {}", p1, p2, p3));

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
                self.commands[index].delta_time = Some(self.commands[index].response_time.unwrap() - self.commands[index].creation_time);
                self.commands[index].response = Some(data);        
                self.commands[index].update_row_vector();
                self.completed_commands.push(self.commands.remove(index));
            },
            None => {}
        }

    }
}

static SPI: Lazy<Mutex<SPIChannel>> = Lazy::new(|| {
//    Mutex::new(SPIChannel::new("/dev/spidev1.1"))
    Mutex::new(SPIChannel::new("/dev/spidev0.0"))
});
/*
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
*/
#[derive(PartialEq)]
enum MessageLevel {
    INFO,
    ERROR,
    CRITICAL
}

impl fmt::Display for MessageLevel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            MessageLevel::INFO => write!(f, "INFO"),
            MessageLevel::ERROR => write!(f, "ERROR"),
            MessageLevel::CRITICAL => write!(f, "CRITICAL")
        }
    }
}

struct Message {
    time: DateTime<Utc>,
    level: MessageLevel,
    string: String
}

struct Messages {
    items: Vec<Message>,
    state: ListState,
    display_size: usize
}

impl Messages {
    fn new() -> Self {
        Messages {
            items: Vec::new(),
            state: ListState::default(),
            display_size: 5
        }
    }

    fn push(&mut self, level: MessageLevel, string: String) {
        self.items.push(Message{ time: Utc::now(), level: level, string: string});
        self.state.select(Some(self.items.len()-1 as usize));
    }
}

static messages: Lazy<Mutex<Messages>> = Lazy::new(|| {
    Mutex::new(Messages::new())
});


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

    let last_command_id = Arc::new(Mutex::new(0u16));

    // Code for seperate thread to generate random events
    let random_event_generator_status: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let reg = random_event_generator_status.clone();
    let last_command_id_thread = last_command_id.clone();
    thread::spawn(move || {
        let mut internal_rng = rand::thread_rng();

        // Approximatly a frequency of 60Hz with a variations that go from 30Hz to 100Hz
        let ln = LogNormal::new(-4.094, 0.2).unwrap();

        loop {
            let val = { *reg.lock().unwrap() };
            match val {
                true => {
                    // Sleep for a random amount
                    let sleep_time = ln.sample(&mut internal_rng) * 1e6;
                    sleep(std::time::Duration::try_from(Duration::microseconds(sleep_time as i64)).unwrap());

                    // Generate random message
                    let length = internal_rng.gen_range(0, 10);
                    let data = (0..length).map(|_| {
                        internal_rng.gen_range(0, 255)
                    }).collect();
                    let cmd_id: u16= {
                        let mut tmp = last_command_id_thread.lock().unwrap();
                        *tmp += 1;
                        *tmp
                    };
                    let cmd = SPICommand::new(cmd_id, 1, data);

                    {
                        let mut SPI_guard = SPI.lock().unwrap();
                        SPI_guard.send_command(cmd);            
                    }
                },
                // Here we just wait till there is something to do...
                false => {
                    sleep(std::time::Duration::try_from(Duration::microseconds(100)).unwrap());
                }
    
            }
        }
    });

    loop {
        terminal.draw(|mut f| {
            let chunks = Layout::default()
            .direction(tui::layout::Direction::Vertical)
            .margin(1)
            .constraints(
                [
                    Constraint::Length(3),
                    Constraint::Length(10),
                    Constraint::Min(20),
                    Constraint::Length(10)
                ].as_ref()
            )
            .split(f.size());

            let texts = [Text::raw(format!("s: Single preformatted echo command          r: Single Shot of Random number of random echo commands        t: Toggle Random Event Generation {}",
                match *random_event_generator_status.lock().unwrap() {
                    true => { "Running" },
                    false => { "Not Running" }
                }
            ))];
            let paragraph = Paragraph::new(
                texts.iter())
                .block(Block::default().borders(Borders::ALL).title("Hotkeys"));
            f.render_widget(paragraph, chunks[0]);

            let incomplete_stye = Style::default().fg(Color::Red);
            let complete_style = Style::default().fg(Color::Green);

            let header = ["Tx Time", "Size", "ID", "Command", "Data", "", "Rx Time", "Delta Time", "Response"];
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

            let rows_completed: Vec<tui::widgets::Row<std::vec::IntoIter<std::string::String>>> = {
                let SPI_guard = SPI.lock().unwrap();
                SPI_guard.completed_commands 
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

            let mut ts = TableState::default();
            ts.select(Some(rows.len()));
            let title = &format!("Unanswered Commands [{}]", rows.len())[..];
            let t = Table::new(header.iter(), rows.into_iter())
                .block(Block::default().borders(Borders::ALL).title(title))
                .widths(&[
                    Constraint::Length(20),
                    Constraint::Length(6),
                    Constraint::Length(6),
                    Constraint::Length(8),
                    Constraint::Min(30),
                    Constraint::Length(8),
                    Constraint::Length(20),
                    Constraint::Length(20),
                    Constraint::Min(30)
                ]);
            f.render_stateful_widget(t, chunks[1], &mut ts);

            let mut ts = TableState::default();
            ts.select(Some(rows_completed.len()));
            let title = &format!("Completed Commands [{}]", rows_completed.len())[..];
            let t = Table::new(header.iter(), rows_completed.into_iter())
                .block(Block::default().borders(Borders::ALL).title(title))
                .widths(&[
                    Constraint::Length(20),
                    Constraint::Length(6),
                    Constraint::Length(6),
                    Constraint::Length(8),
                    Constraint::Min(30),
                    Constraint::Length(8),
                    Constraint::Length(20),
                    Constraint::Length(20),
                    Constraint::Min(30)
                ]);
            f.render_stateful_widget(t, chunks[2], &mut ts);

            // Draw logs
            let info_style = Style::default().fg(Color::White);
            let error_style = Style::default().fg(Color::Magenta);
            let critical_style = Style::default().fg(Color::Red);
            {
                let messages_guard = messages.lock().unwrap();
                let mut state = messages_guard.state.clone();
                let logs = messages_guard.items.iter().map(|item| {
                        Text::styled(
                            format!("{} {}: {}", item.time.format("%H:%M:%S.%f").to_string(), item.level, item.string),
                            match item.level {
                                MessageLevel::INFO => info_style,
                                MessageLevel::CRITICAL => critical_style,
                                MessageLevel::ERROR => error_style
                            },
                        )
                    });

                let logs = List::new(logs).block(Block::default().borders(Borders::ALL).title("Messages"));
                f.render_stateful_widget(logs, chunks[3], &mut state);
            }
        }).unwrap();

        match events.next()? {
            Event::Input(key) => match key {
                Key::Char('q') => {
                    break;
                },
                Key::Char('r') => {
                    {
                        let n = rng.gen_range(1, 10);
                        for i in 0..n {
                            let length = rng.gen_range(0, 5);
                            let data = (0..length).map(|_| {
                                rng.gen_range(0, 255)
                            }).collect();
                            let cmd_id: u16= {
                                let mut tmp = last_command_id.lock().unwrap();
                                *tmp += 1;
                                *tmp
                            };
                            let cmd = SPICommand::new(cmd_id, 0xFFFF, data);

                            {
                                let mut SPI_guard = SPI.lock().unwrap();
                                SPI_guard.send_command(cmd);            
                            }
                        }
                    }
                },
                Key::Char('s') => {

                    let cmd_id: u16= {
                        let mut tmp = last_command_id.lock().unwrap();
                        *tmp += 1;
                        *tmp
                    };
                    let cmd = SPICommand::new(cmd_id, 0xFFFF, vec![0, 1, 2, 3, 4, 5]);

                    let mut SPI_guard = SPI.lock().unwrap();
                    SPI_guard.send_command(cmd);
                },
                Key::Char('e') => {

                    let cmd_id: u16= {
                        let mut tmp = last_command_id.lock().unwrap();
                        *tmp += 1;
                        *tmp
                    };
                    let cmd = SPICommand::new(cmd_id, 0x0001, vec![1]);

                    let mut SPI_guard = SPI.lock().unwrap();
                    SPI_guard.send_command(cmd);
                },
                Key::Char('t') => {
                    let mut tmp = random_event_generator_status.lock().unwrap();
                    *tmp = !*tmp;
                },
                _ => {}
            },
            _ => {}
        };
        // Do event processing here somehow... 
    };

    Ok(())
}
