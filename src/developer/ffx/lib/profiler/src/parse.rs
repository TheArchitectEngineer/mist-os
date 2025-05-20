// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use ffx_symbolize::{MappingDetails, MappingFlags};
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use thiserror::Error;

#[derive(Eq, Hash, PartialEq, Clone, Copy, Debug)]
pub struct Pid(u64);

#[derive(Eq, Hash, PartialEq, Clone, Copy, Debug)]
pub struct Tid(u64);

#[derive(Clone, Debug, PartialEq)]
pub struct ModuleDetails {
    pub name: String,
    pub build_id: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModuleWithMmapDetails {
    pub module: ModuleDetails,
    pub mmaps: Vec<MappingDetails>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BacktraceDetails(pub u64);

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProfilingRecordHandler {
    module_with_mmap_records: Vec<ModuleWithMmapDetails>,
    backtrace_records: HashMap<Tid, Vec<BacktraceDetails>>,
}

impl ProfilingRecordHandler {
    pub fn get_module_with_mmap_records(&self) -> &Vec<ModuleWithMmapDetails> {
        &self.module_with_mmap_records
    }

    pub fn get_backtrace_records(&self) -> &HashMap<Tid, Vec<BacktraceDetails>> {
        &self.backtrace_records
    }

    pub fn parse_markup_line(
        &mut self,
        line: &str,
        tid: Option<Tid>,
    ) -> Result<(), SymbolizeError> {
        if line.starts_with("{{{mmap") {
            if let Some(module_with_mmap_details) = self.module_with_mmap_records.last_mut() {
                module_with_mmap_details.mmaps.push(parse_mmap_record(line)?);
            } else {
                return Err(SymbolizeError::NoModuleRecord(line.to_string()));
            }
        } else if line.starts_with("{{{module") {
            self.module_with_mmap_records
                .push(ModuleWithMmapDetails { module: parse_module_record(line)?, mmaps: vec![] });
        } else if line.starts_with("{{{bt") {
            if let Some(tid) = tid {
                let bt_record = parse_backtrace_record(line)?;
                self.backtrace_records.entry(tid).or_insert_with(Vec::new).push(bt_record);
            } else {
                return Err(SymbolizeError::TidNotExist);
            }
        }
        Ok(())
    }
}

//TODO: convert to use regex
fn parse_mmap_record(record: &str) -> Result<MappingDetails, SymbolizeError> {
    // example input: {{{mmap:0x30db523d000:0x10000:load:0:r:0x0}}}
    let naked_record = record
        .strip_prefix("{{{")
        .and_then(|record| record.strip_suffix("}}}"))
        .ok_or_else(|| SymbolizeError::ParseError {
            record_type: "mmap".to_string(),
            reason: "Failed to strip the prefix or suffix".to_string(),
        })?
        .to_string();
    let parts: Vec<&str> = naked_record.split(':').collect();
    Ok(MappingDetails {
        start_addr: u64::from_str_radix(&parts[1][2..], 16).map_err(|e| {
            SymbolizeError::ParseError {
                record_type: "mmap".to_string(),
                reason: format!("Failed to parse start address: {}", e),
            }
        })?,
        size: u64::from_str_radix(&parts[2][2..], 16).map_err(|e| SymbolizeError::ParseError {
            record_type: "mmap".to_string(),
            reason: format!("Failed to parse size: {}", e),
        })?,
        vaddr: u64::from_str_radix(&parts[6][2..], 16).map_err(|e| SymbolizeError::ParseError {
            record_type: "mmap".to_string(),
            reason: format!("Failed to parse vaddr: {}", e),
        })?,
        flags: MappingFlags::from_str(parts[5]).map_err(|e| SymbolizeError::ParseError {
            record_type: "mmap".to_string(),
            reason: format!("Failed to parse flags: {}", e),
        })?,
    })
}

fn parse_module_record(record: &str) -> Result<ModuleDetails, SymbolizeError> {
    // example: {{{module:0:libtrace-engine.so:elf:333e89f0c175000cee9b7e201fedcd6f9b4ba8ae}}}
    // -> 333e89f0c175000cee9b7e201fedcd6f9b4ba8ae
    let naked_record = record
        .strip_prefix("{{{")
        .and_then(|record| record.strip_suffix("}}}"))
        .ok_or_else(|| SymbolizeError::ParseError {
            record_type: "module".to_string(),
            reason: "Failed to strip the prefix or suffix".to_string(),
        })?
        .to_string();
    let parts: Vec<&str> = naked_record.split(':').collect();
    let build_id = parts
        .last()
        .ok_or_else(|| SymbolizeError::ParseError {
            record_type: "module".to_string(),
            reason: "Failed to get the build id due to it doesn't exist".to_string(),
        })?
        .to_string();
    Ok(ModuleDetails { name: parts[2].to_string(), build_id })
}

fn parse_backtrace_record(record: &str) -> Result<BacktraceDetails, SymbolizeError> {
    // example: {{{bt:0:0x401c0cd1dcea:pc}}} -> u64::from_str_radix(401c0cd1dcea, 16)
    let parts: Vec<&str> = record.split(':').collect();
    Ok(BacktraceDetails(u64::from_str_radix(&parts[2][2..], 16).map_err(|e| {
        SymbolizeError::ParseError {
            record_type: "backtrace".to_string(),
            reason: format!("Failed to get address: {}", e),
        }
    })?))
}

enum ParseStateMachine {
    NotStarted,
    // state will be updated to started when see Reset.
    Start,
    // state will be updated to this after processing a module or mmap record.
    SetModuleOrMmap,
    // state will be updated to this after seeing a pid.
    SetPid { pid: Pid },
    // state will be updated to this after seeing a tid.
    SetPidTid { pid: Pid, tid: Tid },
    // state will be updated to this after see a backtrace.
    SetBacktrace { pid: Pid, tid: Tid },
}

#[derive(PartialEq, Debug)]
pub struct UnsymbolizedSamples {
    pub handlers: HashMap<Pid, ProfilingRecordHandler>,
}

impl UnsymbolizedSamples {
    pub fn new(path: &PathBuf) -> Result<Self, SymbolizeError> {
        let reader = std::fs::read_to_string(path)?;
        let mut profiling_map: HashMap<Pid, ProfilingRecordHandler> = HashMap::new();
        let mut state = ParseStateMachine::NotStarted;
        let mut current_profiling_record_handler = ProfilingRecordHandler::default();
        for line in reader.lines() {
            if line.starts_with("{{{reset") {
                state = ParseStateMachine::Start;
            }
            match state {
                ParseStateMachine::NotStarted => {
                    return Err(SymbolizeError::NoReset);
                }
                ParseStateMachine::Start => {
                    current_profiling_record_handler = ProfilingRecordHandler::default();
                    state = ParseStateMachine::SetModuleOrMmap;
                }
                ParseStateMachine::SetModuleOrMmap => {
                    if line.starts_with("{{{") {
                        current_profiling_record_handler.parse_markup_line(&line, None)?;
                        state = ParseStateMachine::SetModuleOrMmap;
                    } else {
                        // first time see an unique pid
                        let pid = Pid(line.parse::<u64>()?);
                        if profiling_map.contains_key(&pid) {
                            return Err(SymbolizeError::PidAlreadyExist(pid));
                        }
                        profiling_map.insert(pid, current_profiling_record_handler.clone());
                        state = ParseStateMachine::SetPid { pid }
                    }
                }
                ParseStateMachine::SetPid { pid } => {
                    if Pid(line.parse::<u64>()?) != pid {
                        state = ParseStateMachine::SetPidTid { pid, tid: Tid(line.parse::<u64>()?) }
                    } else {
                        return Err(SymbolizeError::NoTid);
                    }
                }
                ParseStateMachine::SetPidTid { pid, tid } => {
                    if !line.starts_with("{{{") {
                        return Err(SymbolizeError::NoBackTrace);
                    }
                    if let Some(profiling_record_handler) = profiling_map.get_mut(&pid) {
                        profiling_record_handler.parse_markup_line(&line, Some(tid))?;
                        state = ParseStateMachine::SetBacktrace { pid, tid }
                    } else {
                        return Err(SymbolizeError::NoPid);
                    }
                }
                ParseStateMachine::SetBacktrace { pid, tid } => {
                    if line.starts_with("{{{") {
                        if let Some(profiling_record_handler) = profiling_map.get_mut(&pid) {
                            profiling_record_handler.parse_markup_line(&line, Some(tid))?;
                        }
                    } else {
                        state = ParseStateMachine::SetPid { pid }
                    }
                }
            }
        }
        Ok(UnsymbolizedSamples { handlers: profiling_map })
    }
}

#[derive(Error, Debug)]
pub enum SymbolizeError {
    #[error("TID is not set for the current backtrace.")]
    TidNotExist,

    #[error("Failed to load ffx environment context.")]
    NoFfxEnvironmentContext,

    #[error("The current pid {:?} already been processed. Please verify the record.", .0)]
    PidAlreadyExist(Pid),

    #[error("Failed to parse the {} record due to {}.", record_type, reason)]
    ParseError { record_type: String, reason: String },

    #[error("Failed to open the profiler file due to {}", .0)]
    FileError(#[from] std::io::Error),

    #[error("Failed to convert string to u32 for pid or tid due to {}", .0)]
    PidOrTidConvertError(#[from] std::num::ParseIntError),

    #[error("A reset is expected at the begin of the profile file.")]
    NoReset,

    #[error("Tid not found.")]
    NoTid,

    #[error("BackTrace not found.")]
    NoBackTrace,

    #[error("Pid is not set yet for the current backtrace")]
    NoPid,

    #[error("No module record associate with the mmap record: {}.", .0)]
    NoModuleRecord(String),

    #[error("Failed to create symbolizer due to {}", .0)]
    SymbolizerError(#[from] ffx_symbolize::CreateSymbolizerError),

    #[error("Failed to add mapping due to {}", .0)]
    AddMappingError(#[from] ffx_symbolize::AddMappingError),

    #[error("Failed to resolve address due to {}", .0)]
    ResolveAddressError(#[from] ffx_symbolize::ResolveError),

    #[error("Failed to convert string to u64 due to {}", .0)]
    HexConvertError(#[from] hex::FromHexError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_set_up_profiling_record_handlers() {
        let mut profiler_record = NamedTempFile::new().expect("Failed to create temp file");
        let profiler_record_content = "{{{reset}}}
{{{module:0:libtrace-engine.so:elf:333e89f0c175000cee9b7e201fedcd6f9b4ba8ae}}}
{{{mmap:0xc936396000:0x6000:load:0:r:0x0}}}
{{{mmap:0xc93639c000:0xd000:load:0:rx:0x6000}}}
1104
2616
{{{bt:0:0x43dc387f8e10:pc}}}
{{{bt:1:0x2b069ffa16c:ra}}}
1104
1226
{{{bt:0:0x43dc387f8e10:pc}}}
{{{bt:1:0x3a656c4c85e:ra}}}
{{{reset}}}
{{{module:0:<VMO#4165=/boot/bin/sh>:elf:867c18818584f5823f35472b70fc8714b2518ba0}}}
{{{mmap:0x30db523d000:0x10000:load:0:r:0x0}}}
{{{module:1:libfdio.so:elf:3e1c4eb82f79af6a4fd142db11f83979772f9867}}}
{{{mmap:0x3bfd82b3000:0x62000:load:1:r:0x0}}}
4207
4209
{{{bt:0:0x401c0cd1dcea:pc}}}
{{{bt:1:0x3bfd834db94:ra}}}
4207
4209
{{{bt:0:0x401c0cd1dcea:pc}}}
{{{bt:1:0x3bfd834db94:ra}}}
{{{bt:2:0x3bfd834e80b:ra}}}";
        writeln!(profiler_record, "{}", profiler_record_content)
            .expect("Failed to write to temp file");
        profiler_record.flush().expect("Failed to flush");
        let profiler_record_path: PathBuf = profiler_record.path().to_path_buf();
        let handlers = UnsymbolizedSamples::new(&profiler_record_path).unwrap();
        let first_handler = ProfilingRecordHandler {
            module_with_mmap_records: vec![ModuleWithMmapDetails {
                module: ModuleDetails {
                    name: "libtrace-engine.so".to_string(),
                    build_id: "333e89f0c175000cee9b7e201fedcd6f9b4ba8ae".to_string(),
                },
                mmaps: vec![
                    MappingDetails {
                        start_addr: 0xc936396000,
                        size: 0x6000,
                        vaddr: 0x0,
                        flags: MappingFlags::READ,
                    },
                    MappingDetails {
                        start_addr: 0xc93639c000,
                        size: 0xd000,
                        vaddr: 0x6000,
                        flags: MappingFlags::READ | MappingFlags::EXECUTE,
                    },
                ],
            }],
            backtrace_records: HashMap::from([
                (
                    Tid(2616),
                    vec![BacktraceDetails(0x43dc387f8e10), BacktraceDetails(0x2b069ffa16c)],
                ),
                (
                    Tid(1226),
                    vec![BacktraceDetails(0x43dc387f8e10), BacktraceDetails(0x3a656c4c85e)],
                ),
            ]),
        };

        let second_handler = ProfilingRecordHandler {
            module_with_mmap_records: vec![
                ModuleWithMmapDetails {
                    module: ModuleDetails {
                        name: "<VMO#4165=/boot/bin/sh>".to_string(),
                        build_id: "867c18818584f5823f35472b70fc8714b2518ba0".to_string(),
                    },
                    mmaps: vec![MappingDetails {
                        start_addr: 0x30db523d000,
                        size: 0x10000,
                        vaddr: 0x0,
                        flags: MappingFlags::READ,
                    }],
                },
                ModuleWithMmapDetails {
                    module: ModuleDetails {
                        name: "libfdio.so".to_string(),
                        build_id: "3e1c4eb82f79af6a4fd142db11f83979772f9867".to_string(),
                    },
                    mmaps: vec![MappingDetails {
                        start_addr: 0x3bfd82b3000,
                        size: 0x62000,
                        vaddr: 0x0,
                        flags: MappingFlags::READ,
                    }],
                },
            ],
            backtrace_records: HashMap::from([(
                Tid(4209),
                vec![
                    BacktraceDetails(0x401c0cd1dcea),
                    BacktraceDetails(0x3bfd834db94),
                    BacktraceDetails(0x401c0cd1dcea),
                    BacktraceDetails(0x3bfd834db94),
                    BacktraceDetails(0x3bfd834e80b),
                ],
            )]),
        };
        let mut expected_handlers = HashMap::new();
        expected_handlers.insert(Pid(4207), second_handler);
        expected_handlers.insert(Pid(1104), first_handler);
        assert_eq!(UnsymbolizedSamples { handlers: expected_handlers }, handlers);
    }
}
