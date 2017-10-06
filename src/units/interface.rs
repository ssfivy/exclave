extern crate runny;
extern crate systemd_parser;

use std::cell::RefCell;
use std::fs::File;
use std::io::{Read, Write, BufRead, BufReader};
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use config::Config;
use unit::{UnitActivateError, UnitDeactivateError, UnitDescriptionError, UnitIncompatibleReason,
           UnitName};
use unitmanager::{UnitManager, ManagerStatusMessage, ManagerControlMessage, ManagerControlMessageContents};

use self::systemd_parser::items::DirectiveEntry;
use self::runny::{Runny};
use self::runny::running::{Running, RunningOutput};

#[derive(Clone, Copy)]
enum InterfaceFormat {
    Text,
    JSON,
}

/// A struct defining an in-memory representation of a .Interface file
pub struct InterfaceDescription {
    /// The id of the unit (including the kind)
    id: UnitName,

    /// A short name
    name: String,

    /// A detailed description of this Interface, up to one paragraph.
    description: String,

    /// A Vec<String> of jig names that this test is compatible with.
    jigs: Vec<UnitName>,

    /// Path to the command to start the interface
    exec_start: String,

    /// The format expected by the interface
    format: InterfaceFormat,
}

impl InterfaceDescription {
    pub fn from_path(path: &Path) -> Result<InterfaceDescription, UnitDescriptionError> {
        let unit_name = UnitName::from_path(path)?;

        // Parse the file into a systemd unit_file object
        let mut contents = String::with_capacity(8192);
        File::open(path)?.read_to_string(&mut contents)?;
        let unit_file = systemd_parser::parse_string(&contents)?;

        if !unit_file.has_category("Interface") {
            return Err(UnitDescriptionError::MissingSection("Interface".to_owned()));
        }

        let mut interface_description = InterfaceDescription {
            id: unit_name,
            name: "".to_owned(),
            description: "".to_owned(),
            jigs: vec![],
            format: InterfaceFormat::Text,
            exec_start: "".to_owned(),
        };

        for entry in unit_file.lookup_by_category("Interface") {
            match entry {
                &DirectiveEntry::Solo(ref directive) => match directive.key() {
                    "Name" => {
                        interface_description.name = directive.value().unwrap_or("").to_owned()
                    }
                    "Description" => {
                        interface_description.description =
                            directive.value().unwrap_or("").to_owned()
                    }
                    "Jigs" => {
                        interface_description.jigs = match directive.value() {
                            Some(s) => UnitName::from_list(s, "jig")?,
                            None => vec![],
                        }
                    }
                    "ExecStart" => {
                        interface_description.exec_start = match directive.value() {
                            Some(s) => s.to_owned(),
                            None => {
                                return Err(UnitDescriptionError::MissingValue(
                                    "Interface".to_owned(),
                                    "ExecStart".to_owned(),
                                ))
                            }
                        }
                    }
                    "Format" => {
                        interface_description.format = match directive.value() {
                            None => InterfaceFormat::Text,
                            Some(s) => match s.to_string().to_lowercase().as_ref() {
                                "text" => InterfaceFormat::Text,
                                "json" => InterfaceFormat::JSON,
                                other => {
                                    return Err(UnitDescriptionError::InvalidValue(
                                        "Interface".to_owned(),
                                        "Format".to_owned(),
                                        other.to_owned(),
                                        vec!["text".to_owned(), "json".to_owned()],
                                    ))
                                }
                            },
                        }
                    }
                    &_ => (),
                },
                &_ => (),
            }
        }
        Ok(interface_description)
    }

    /// Returns true if this test is supported on the named jig.
    pub fn supports_jig(&self, name: &UnitName) -> bool {
        self.jigs.contains(name)
    }

    /// Determine if a unit is compatible with this system.
    pub fn is_compatible(
        &self,
        manager: &UnitManager,
        _: &Config,
    ) -> Result<(), UnitIncompatibleReason> {
        if self.jigs.len() == 0 {
            return Ok(());
        }
        for jig_name in &self.jigs {
            if manager.jig_is_loaded(&jig_name) {
                return Ok(());
            }
        }
        Err(UnitIncompatibleReason::IncompatibleJig)
    }

    pub fn id(&self) -> &UnitName {
        &self.id
    }

    pub fn select(
        &self,
        manager: &UnitManager,
        config: &Config,
    ) -> Result<Interface, UnitIncompatibleReason> {
        self.is_compatible(manager, config)?;

        Ok(Interface::new(self, manager, config))
    }
}

pub struct Interface {
    id: UnitName,
    exec_start: String,
    format: InterfaceFormat,
    process: RefCell<Option<Running>>,
}

impl Interface {
    pub fn new(desc: &InterfaceDescription, manager: &UnitManager, config: &Config) -> Interface {
        Interface {
            id: desc.id.clone(),
            exec_start: desc.exec_start.clone(),
            format: desc.format,
            process: RefCell::new(None),
        }
    }

    pub fn id(&self) -> &UnitName {
        &self.id
    }

    pub fn activate(
        &self,
        manager: &UnitManager,
        config: &Config,
    ) -> Result<Receiver<ManagerControlMessage>, UnitActivateError> {
        let (sender, receiver) = channel();

        let mut running = Runny::new(self.exec_start.as_str())
            .directory(&Some(config.working_directory().clone()))
            .start()?;

        let stdout = running.take_output();

        let control_sender = manager.get_control_channel();
        let control_sender_id = self.id().clone();
        match self.format {
            InterfaceFormat::Text => {

                // Send some initial information to the client.
                writeln!(running, "HELLO Jig/20 1.0").unwrap();

                thread::spawn(move || Self::text_read(control_sender_id, control_sender, stdout));
            }
            InterfaceFormat::JSON => {
                ();
            }
        };

        *self.process.borrow_mut() = Some(running);

        Ok(receiver)
    }

    pub fn deactivate(&self) -> Result<(), UnitDeactivateError> {
        Ok(())
    }

    /// Write a UnitInterfaceMessage to a Text-formatted output.
    pub fn text_write(&self, msg: ManagerStatusMessage) -> Result<(), String>
    {
        let mut process_opt = self.process.borrow_mut();

        if process_opt.is_none() {
            return Err("No process running".to_owned());
        }

        let process = process_opt.as_mut().unwrap();

        let result = match msg {
            ManagerStatusMessage::Jig(j) => writeln!(process, "JIG {}", j.to_string()),
            /*
            BroadcastMessageContents::Log(l) => writeln!(
                stdin,
                "LOG {}\t{}\t{}\t{}\t{}\t{}",
                msg.message_class,
                msg.unit_id,
                msg.unit_type,
                msg.unix_time,
                msg.unix_time_nsecs,
                l.to_string()
                    .replace("\\", "\\\\")
                    .replace("\t", "\\t")
                    .replace("\n", "\\n")
                    .replace("\r", "\\r")
            ),
            BroadcastMessageContents::Describe(class, field, name, value) => {
                writeln!(stdin, "DESCRIBE {} {} {} {}", class, field, name, value)
            }
            BroadcastMessageContents::Scenario(name) => writeln!(stdin, "SCENARIO {}", name),
            BroadcastMessageContents::Scenarios(list) => {
                writeln!(stdin, "SCENARIOS {}", list.join(" "))
            }
            //            BroadcastMessageContents::Hello(name) => writeln!(stdin,
            //                                                "HELLO {}", name),
            //            BroadcastMessageContents::Ping(val) => writeln!(stdin,
            //                                                "PING {}", val),
            BroadcastMessageContents::Shutdown(reason) => writeln!(stdin, "EXIT {}", reason),
            BroadcastMessageContents::Tests(scenario, tests) => {
                writeln!(stdin, "TESTS {} {}", scenario, tests.join(" "))
            }
            BroadcastMessageContents::Running(test) => writeln!(stdin, "RUNNING {}", test),
            BroadcastMessageContents::Skip(test, reason) => {
                writeln!(stdin, "SKIP {} {}", test, reason)
            }
            BroadcastMessageContents::Fail(test, reason) => {
                writeln!(stdin, "FAIL {} {}", test, reason)
            }
            BroadcastMessageContents::Pass(test, reason) => {
                writeln!(stdin, "PASS {} {}", test, reason)
            }
            BroadcastMessageContents::Start(scenario) => writeln!(stdin, "START {}", scenario),
            BroadcastMessageContents::Finish(scenario, result, reason) => {
                writeln!(stdin, "FINISH {} {} {}", scenario, result, reason)
            }
            */
        };
        match result {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("{:?}", e)),
        }
    }

    fn cfti_unescape(msg: String) -> String {
        msg.replace("\\t", "\t").replace("\\n", "\n").replace("\\r", "\r").replace("\\\\", "\\")
    }

    fn text_read(id: UnitName, control: Sender<ManagerControlMessage>, stdout: RunningOutput) {
        for line in BufReader::new(stdout).lines() {
            let line = line.expect("Unable to get next line");
            let mut words: Vec<String> =
                line.split_whitespace().map(|x| Self::cfti_unescape(x.to_string())).collect();

            // Don't crash if we get a blank line.
            if words.len() == 0 {
                continue;
            }

            let verb = words[0].to_lowercase();
            words.remove(0);

            let response = match verb.as_str() {
                "scenarios" => ManagerControlMessageContents::Scenarios,
                /*
                "scenario" => ControlMessageContents::Scenario(words[0].to_lowercase()),
                "tests" => {
                    if words.is_empty() {
                        ControlMessageContents::GetTests(None)
                    } else {
                        ControlMessageContents::GetTests(Some(words[0].to_lowercase()))
                    }
                }
                "start" => {
                    if words.is_empty() {
                        ControlMessageContents::StartScenario(None)
                    } else {
                        ControlMessageContents::StartScenario(Some(words[0].to_lowercase()))
                    }
                }
                "abort" => ControlMessageContents::AbortTests,
                "pong" => ControlMessageContents::Pong(words[0].to_lowercase()),
                "jig" => ControlMessageContents::GetJig,
                "hello" => ControlMessageContents::Hello(words.join(" ")),
                "shutdown" => {
                    if words.is_empty() {
                        ControlMessageContents::Shutdown(None)
                    } else {
                        ControlMessageContents::Shutdown(Some(words.join(" ")))
                    }
                }
                "log" => ControlMessageContents::Log(words.join(" ")),
                */
                v => ManagerControlMessageContents::Unimplemented(format!("Unimplemented verb: {}", v)),
            };

            // If the send fails, that means the other end has closed the pipe.
            if let Err(_) = control.send(ManagerControlMessage::new(&id, response)) {
                return;
            }
        }
    }
}