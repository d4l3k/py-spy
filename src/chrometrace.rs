use std::cmp::min;
use std::collections::HashMap;
use std::io::Write;
use std::time::Instant;

use anyhow::Error;
use serde_derive::Serialize;

use crate::stack_trace::Frame;
use crate::stack_trace::StackTrace;

#[derive(Clone, Debug, Serialize)]
struct Args {
    pub filename: String,
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct Event {
    pub args: Args,
    pub cat: String,
    pub name: String,
    pub ph: String,
    pub pid: u64,
    pub tid: u32,
    pub ts: u64,
}

pub struct Chrometrace {
    events: Vec<Event>,
    start_ts: Instant,
    prev_traces: HashMap<u64, StackTrace>,
    show_linenumbers: bool,
    // Perfetto only supports 32bit thread IDs so we remap them in the actual emitted events.
    thread_ids: HashMap<u64, u32>,
}

impl Chrometrace {
    pub fn new(show_linenumbers: bool) -> Chrometrace {
        Chrometrace {
            events: Vec::new(),
            start_ts: Instant::now(),
            prev_traces: HashMap::new(),
            show_linenumbers,
            thread_ids: HashMap::new(),
        }
    }

    // Return whether these frames are similar enough such that we should merge
    // them, instead of creating separate events for them.
    fn should_merge_frames(&self, a: &Frame, b: &Frame) -> bool {
        a.name == b.name && a.filename == b.filename && (!self.show_linenumbers || a.line == b.line)
    }

    fn event(&self, trace: &StackTrace, frame: &Frame, phase: &str, ts: u64) -> Event {
        Event {
            tid: self.get_thread_id(trace),
            pid: trace.pid as u64,
            name: frame.name.to_string(),
            cat: "py-spy".to_owned(),
            ph: phase.to_owned(),
            ts,
            args: Args {
                filename: frame.filename.to_string(),
                line: if self.show_linenumbers {
                    Some(frame.line as u32)
                } else {
                    None
                },
                name: None,
            },
        }
    }

    fn get_thread_id<'a>(&self, trace: &'a StackTrace) -> u32 {
        let thread_id = trace.thread_id;
        self.thread_ids[&thread_id]
    }

    fn record_new_thread(&mut self, trace: &StackTrace) {
        if !self.thread_ids.contains_key(&trace.thread_id) {
            let thread_id = trace.thread_id;

            // remap IDs to be in the uint32 space
            let remapped_id = self.thread_ids.len() as u32;
            self.thread_ids.insert(thread_id, remapped_id);

            let name = thread_id.to_string() + ": " + trace.thread_name .as_deref().unwrap_or_default();

            self.events.push(Event { 
                args: Args { filename: "".to_owned(), line: None, name: Some(name)}, 
                cat: "py-spy".to_owned(), 
                name: "thread_name".to_owned(), 
                ph: "M".to_owned(), 
                pid: trace.pid as u64, 
                tid: self.get_thread_id(trace),
                ts: 0,
            });
        }
    }

    pub fn increment(&mut self, trace: &StackTrace) -> std::io::Result<()> {
        let now = self.start_ts.elapsed().as_micros() as u64;

        // Maybe add metadata for new threads.
        self.record_new_thread(trace);

        // Load the previous frames for this thread.
        let prev_frames = self
            .prev_traces
            .remove(&trace.thread_id)
            .map(|t| t.frames)
            .unwrap_or_default();

        // Find the index where we first see new frames.
        let new_idx = prev_frames
            .iter()
            .rev()
            .zip(trace.frames.iter().rev())
            .position(|(a, b)| !self.should_merge_frames(a, b))
            .unwrap_or(min(prev_frames.len(), trace.frames.len()));

        // Publish end events for the previous frames that got dropped in the
        // most recent trace.
        for frame in prev_frames.iter().rev().skip(new_idx).rev() {
            self.events.push(self.event(trace, frame, "E", now));
        }

        // Publish start events for frames that got added in the most recent
        // trace.
        for frame in trace.frames.iter().rev().skip(new_idx) {
            self.events.push(self.event(trace, frame, "B", now));
        }

        // Save this stack trace for the next iteration.
        self.prev_traces.insert(trace.thread_id, trace.clone());

        Ok(())
    }

    pub fn write(&self, w: &mut dyn Write) -> Result<(), Error> {
        let mut events = Vec::new();
        events.extend(self.events.to_vec());

        // Add end events for any unfinished slices.
        let now = self.start_ts.elapsed().as_micros() as u64;
        for trace in self.prev_traces.values() {
            for frame in &trace.frames {
                events.push(self.event(trace, frame, "E", now));
            }
        }

        writeln!(w, "{}", serde_json::to_string(&events)?)?;
        Ok(())
    }
}
