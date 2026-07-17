// Version: v1.0 (Final Version) Made by Lukdi Midi With Gemini
mod midi_parser;

use std::fs::{File, metadata};
use std::io::{self, Read, Seek, SeekFrom, BufReader};
use std::collections::BinaryHeap;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use native_dialog::FileDialog;
use xz2::read::XzDecoder;

#[derive(Clone, PartialEq, Eq)]
struct NextEvent {
    tick: u32,
    track_idx: usize,
}

impl Ord for NextEvent {
    #[inline(always)]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.tick.cmp(&self.tick) 
    }
}

impl PartialOrd for NextEvent {
    #[inline(always)]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

struct MidiExtendedInfo {
    filename: String,
    file_size_mib: f64,
    ppqn: u16,
    tracks_count: usize,
    total_notes: u64,
    max_polyphony: i32,
    max_nps: u64,
    total_cc_events: u64,
    duration_secs: f64,
}

#[derive(Debug, Clone)]
struct TempoChange {
    abs_tick: u32,
    tempo_us: u64,
    abs_micros: f64, 
}

struct TempoMap {
    changes: Vec<TempoChange>,
}

impl TempoMap {
    fn new() -> Self {
        TempoMap { changes: Vec::new() }
    }

    fn build_timeline(&mut self, ppqn: u16) {
        if self.changes.is_empty() { return; }
        self.changes.sort_by_key(|c| c.abs_tick);

        let tpb = ppqn as f64;
        let mut current_micros = 0.0f64;
        let mut last_tick = 0u32;
        let mut current_tempo = 500_000u64; 

        for change in self.changes.iter_mut() {
            let delta_ticks = (change.abs_tick - last_tick) as f64;
            current_micros += (delta_ticks * current_tempo as f64) / tpb;
            
            change.abs_micros = current_micros;
            last_tick = change.abs_tick;
            current_tempo = change.tempo_us;
        }
    }

    #[inline(always)]
    fn tick_to_secs(&self, tick: u32, ppqn: u16) -> f64 {
        if self.changes.is_empty() {
            return (tick as f64 * 500_000.0) / (ppqn as f64 * 1_000_000.0);
        }

        let idx = match self.changes.binary_search_by_key(&tick, |c| c.abs_tick) {
            Ok(found) => found,
            Err(inserted) => inserted.saturating_sub(1),
        };

        let base = &self.changes[idx];
        if tick >= base.abs_tick {
            let delta_ticks = (tick - base.abs_tick) as f64;
            let extra_micros = (delta_ticks * base.tempo_us as f64) / (ppqn as f64);
            (base.abs_micros + extra_micros) / 1_000_000.0
        } else {
            (tick as f64 * 500_000.0) / (ppqn as f64 * 1_000_000.0)
        }
    }
}

struct BufferedTrackCursor {
    track_idx: usize,
    file: File,
    buffer: Vec<u8>,
    buffer_start_file_pos: u64,
    buffer_len: usize,
    buffer_capacity: usize,
    file_track_end: u64,
    file_current_pos: u64,
    abs_tick: u32,
    last_status: u8,
}

impl BufferedTrackCursor {
    fn new(mut file: File, start: u64, end: u64, idx: usize, max_buffer_size: usize) -> Self {
        let track_len = (end - start) as usize;
        let actual_buffer_capacity = max_buffer_size.min(track_len).max(1024);
        let mut cursor = BufferedTrackCursor {
            track_idx: idx, file, file_track_end: end, file_current_pos: start,
            buffer: vec![0u8; actual_buffer_capacity], buffer_start_file_pos: 0, buffer_len: 0,
            buffer_capacity: actual_buffer_capacity, abs_tick: 0, last_status: 0,
        };
        let _ = cursor.fill_buffer();
        cursor
    }

    fn fill_buffer(&mut self) -> io::Result<()> {
        if self.file_current_pos >= self.file_track_end { self.buffer_len = 0; return Ok(()); }
        let remaining = (self.file_track_end - self.file_current_pos) as usize;
        let to_read = self.buffer_capacity.min(remaining);
        self.file.seek(SeekFrom::Start(self.file_current_pos))?;
        self.file.read_exact(&mut self.buffer[..to_read])?;
        self.buffer_start_file_pos = self.file_current_pos;
        self.buffer_len = to_read;
        Ok(())
    }

    #[inline(always)]
    fn has_data(&self) -> bool {
        self.file_current_pos < self.file_track_end
    }

    #[inline(always)]
    fn read_byte(&mut self) -> u8 {
        let local_pos = (self.file_current_pos - self.buffer_start_file_pos) as usize;
        if local_pos >= self.buffer_len { let _ = self.fill_buffer(); }
        let b = self.buffer[(self.file_current_pos - self.buffer_start_file_pos) as usize];
        self.file_current_pos += 1;
        b
    }

    #[inline(always)]
    fn peek_byte(&mut self) -> u8 {
        let local_pos = (self.file_current_pos - self.buffer_start_file_pos) as usize;
        if local_pos >= self.buffer_len { let _ = self.fill_buffer(); }
        self.buffer[(self.file_current_pos - self.buffer_start_file_pos) as usize]
    }

    #[inline(always)]
    fn skip_bytes(&mut self, len: u64) {
        self.file_current_pos += len;
        let local_pos = (self.file_current_pos - self.buffer_start_file_pos) as usize;
        if local_pos >= self.buffer_len && self.file_current_pos < self.file_track_end {
            let _ = self.fill_buffer();
        }
    }
}

#[inline(always)]
fn read_varint_buffered(c: &mut BufferedTrackCursor) -> u32 {
    let mut value: u32 = 0;
    loop {
        let b = c.read_byte();
        value = (value << 7) | ((b & 0x7F) as u32);
        if (b & 0x80) == 0 { break; }
    }
    value
}

struct ParsedMidiEvent {
    abs_tick: u32,
    is_note_on: bool,
    is_cc: bool,
}

#[inline(always)]
fn read_varint_from_stream<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut value: u32 = 0;
    let mut b = [0u8; 1];
    loop {
        r.read_exact(&mut b)?;
        value = (value << 7) | ((b[0] & 0x7F) as u32);
        if (b[0] & 0x80) == 0 { break; }
    }
    Ok(value)
}

struct ChannelReader {
    rx: mpsc::Receiver<Vec<u8>>,
    current_chunk: Vec<u8>,
    pos: usize,
    peeked_byte: Option<u8>, 
}

impl ChannelReader {
    fn unget_byte(&mut self, b: u8) {
        self.peeked_byte = Some(b);
    }
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() { return Ok(0); }
        let mut bytes_read = 0;
        if let Some(b) = self.peeked_byte.take() {
            buf[0] = b;
            if buf.len() == 1 { return Ok(1); }
            bytes_read += 1;
        }
        if self.pos >= self.current_chunk.len() {
            match self.rx.recv() {
                Ok(chunk) => { self.current_chunk = chunk; self.pos = 0; }
                Err(_) => return Ok(bytes_read),
            }
        }
        let available = self.current_chunk.len() - self.pos;
        let to_copy = available.min(buf.len() - bytes_read);
        buf[bytes_read..bytes_read + to_copy].copy_from_slice(&self.current_chunk[self.pos..self.pos + to_copy]);
        self.pos += to_copy;
        Ok(bytes_read + to_copy)
    }
}

fn extract_standard_midi_info(path: &Path, original_filename: String) -> Result<MidiExtendedInfo, Box<dyn std::error::Error>> {
    let file_len = metadata(path)?.len();
    let file_size_mib = file_len as f64 / (1024.0 * 1024.0);

    let mut setup_file = File::open(path)?;
    let mut header = [0u8; 14];
    setup_file.read_exact(&mut header)?;

    if &header[0..4] != b"MThd" {
        return Err("Invalid MIDI file: MThd missing".into());
    }

    let division = u16::from_be_bytes([header[12], header[13]]);
    let tpb = division as f64; 

    let mut track_offsets = Vec::new();
    let mut pos = 14u64;
    let mut search_buf = [0u8; 8];
    
    while pos < file_len {
        setup_file.seek(SeekFrom::Start(pos))?;
        if pos + 8 <= file_len {
            setup_file.read_exact(&mut search_buf)?;
            if &search_buf[0..4] == b"MTrk" {
                let len = u32::from_be_bytes([search_buf[4], search_buf[5], search_buf[6], search_buf[7]]) as u64;
                track_offsets.push((pos + 8, pos + 8 + len));
                pos = pos + 8 + len;
                continue;
            }
        }
        pos += 1;
    }

    let tracks_count = track_offsets.len();
    
    let max_track_buffer_size = if tracks_count > 4096 {
        ((2 * 1024 * 1024 * 1024) / tracks_count).min(1024 * 1024).max(64 * 1024)
    } else {
        1024 * 1024
    };

    let mut cursors = Vec::with_capacity(tracks_count);
    for (idx, &(start, end)) in track_offsets.iter().enumerate() {
        cursors.push(BufferedTrackCursor::new(File::open(path)?, start, end, idx, max_track_buffer_size));
    }

    let mut heap = BinaryHeap::with_capacity(tracks_count);
    for c in &mut cursors {
        if c.has_data() {
            let delta = read_varint_buffered(c);
            c.abs_tick = delta;
            heap.push(NextEvent { tick: c.abs_tick, track_idx: c.track_idx });
        }
    }

    let mut total_notes = 0u64;
    let mut current_polyphony = 0i32;
    let mut max_polyphony = 0i32;
    let mut total_cc_events = 0u64;
    let mut current_micros = 0.0f64;
    let mut last_time_tick = 0u32;
    let mut current_tempo_us = 500_000u64; 

    let mut max_nps = 0u64;
    let mut notes_in_current_second = 0u64;
    let mut window_start_micros = 0.0f64;

    while let Some(event) = heap.pop() {
        let c = &mut cursors[event.track_idx];
        
        if event.tick > last_time_tick {
            let delta_ticks = (event.tick - last_time_tick) as f64;
            current_micros += (delta_ticks * current_tempo_us as f64) / tpb;
            last_time_tick = event.tick;
        }

        if current_micros - window_start_micros >= 1_000_000.0 {
            if notes_in_current_second > max_nps { max_nps = notes_in_current_second; }
            notes_in_current_second = 0;
            window_start_micros = current_micros;
        }

        if !c.has_data() { continue; }

        let next_byte = c.peek_byte();
        let mut status = next_byte;
        if status >= 0x80 {
            let _ = c.read_byte(); 
            c.last_status = status;
        } else {
            status = c.last_status; 
        }

        let msg_type = status & 0xF0;

        if msg_type == 0x90 { 
            let _note = c.read_byte();
            let velocity = c.read_byte();
            if velocity > 0 {
                total_notes += 1;
                notes_in_current_second += 1;
                current_polyphony += 1;
                if current_polyphony > max_polyphony { max_polyphony = current_polyphony; }
            } else {
                current_polyphony = (current_polyphony - 1).max(0);
            }
        } else if msg_type == 0x80 { 
            let _note = c.read_byte();
            let _vel = c.read_byte();
            current_polyphony = (current_polyphony - 1).max(0);
        } else if msg_type == 0xB0 { 
            total_cc_events += 1;
            c.skip_bytes(2);
        } else if msg_type == 0xA0 || msg_type == 0xE0 {
            c.skip_bytes(2);
        } else if msg_type == 0xC0 || msg_type == 0xD0 {
            c.skip_bytes(1);
        } else if status == 0xFF { 
            let meta_type = c.read_byte();
            let meta_len = read_varint_buffered(c) as u64;
            
            if meta_type == 0x51 && meta_len == 3 {
                let t0 = c.read_byte() as u64;
                let t1 = c.read_byte() as u64;
                let t2 = c.read_byte() as u64;
                current_tempo_us = (t0 << 16) | (t1 << 8) | t2;
            } else {
                c.skip_bytes(meta_len);
            }
        } else if status == 0xF0 || status == 0xF7 { 
            let sys_len = read_varint_buffered(c) as u64;
            c.skip_bytes(sys_len);
        }

        if c.has_data() {
            let next_delta = read_varint_buffered(c);
            c.abs_tick = c.abs_tick.saturating_add(next_delta);
            heap.push(NextEvent { tick: c.abs_tick, track_idx: c.track_idx });
        }
    }

    if notes_in_current_second > max_nps { max_nps = notes_in_current_second; }
    let duration_secs = current_micros / 1_000_000.0;

    Ok(MidiExtendedInfo {
        filename: original_filename, file_size_mib, ppqn: division, tracks_count,
        total_notes, max_polyphony, max_nps, total_cc_events, duration_secs,
    })
}

fn extract_xz_parallel_stream(path: &Path, original_filename: String) -> Result<MidiExtendedInfo, Box<dyn std::error::Error>> {
    let file_len = metadata(path)?.len();
    let file_size_mib = file_len as f64 / (1024.0 * 1024.0);

    let mut tempo_map = TempoMap::new();
    let mut division = 96u16;
    let mut t_count = 0;

    {
        let file = File::open(path)?;
        let mut decoder = XzDecoder::new(BufReader::new(file));
        let mut header = [0u8; 14];
        decoder.read_exact(&mut header)?;
        division = u16::from_be_bytes([header[12], header[13]]);

        let mut chunk_tag = [0u8; 4];

        loop {
            if decoder.read_exact(&mut chunk_tag).is_err() { break; }
            if &chunk_tag == b"MTrk" {
                t_count += 1;
                let mut len_bytes = [0u8; 4];
                decoder.read_exact(&mut len_bytes)?;
                let track_len = u32::from_be_bytes(len_bytes) as usize;

                let mut track_data = vec![0u8; track_len];
                decoder.read_exact(&mut track_data)?;

                let mut offset = 0;
                let mut abs_tick = 0u32;
                let mut last_status = 0u8;

                while offset < track_len {
                    let mut delta: u32 = 0;
                    loop {
                        if offset >= track_len { break; }
                        let b = track_data[offset];
                        offset += 1;
                        delta = (delta << 7) | ((b & 0x7F) as u32);
                        if (b & 0x80) == 0 { break; }
                    }
                    abs_tick = abs_tick.saturating_add(delta);

                    if offset >= track_len { break; }
                    let mut status = track_data[offset];
                    offset += 1;

                    if status >= 0x80 {
                        last_status = status;
                    } else {
                        status = last_status;
                        offset -= 1;
                    }

                    let msg_type = status & 0xF0;
                    if msg_type == 0x90 || msg_type == 0x80 || msg_type == 0xB0 || msg_type == 0xA0 || msg_type == 0xE0 {
                        offset += 2;
                    } else if msg_type == 0xC0 || msg_type == 0xD0 {
                        offset += 1;
                    } else if status == 0xFF {
                        if offset >= track_len { break; }
                        let meta_type = track_data[offset];
                        offset += 1;

                        let mut meta_len: u32 = 0;
                        loop {
                            if offset >= track_len { break; }
                            let b = track_data[offset];
                            offset += 1;
                            meta_len = (meta_len << 7) | ((b & 0x7F) as u32);
                            if (b & 0x80) == 0 { break; }
                        }

                        if meta_type == 0x51 && meta_len == 3 && offset + 3 <= track_len {
                            let t0 = track_data[offset] as u64;
                            let t1 = track_data[offset + 1] as u64;
                            let t2 = track_data[offset + 2] as u64;
                            let tempo_us = (t0 << 16) | (t1 << 8) | t2;
                            tempo_map.changes.push(TempoChange { abs_tick, tempo_us, abs_micros: 0.0 });
                        }
                        offset += meta_len as usize;
                    } else if status == 0xF0 || status == 0xF7 {
                        let mut sys_len: u32 = 0;
                        loop {
                            if offset >= track_len { break; }
                            let b = track_data[offset];
                            offset += 1;
                            sys_len = (sys_len << 7) | ((b & 0x7F) as u32);
                            if (b & 0x80) == 0 { break; }
                        }
                        offset += sys_len as usize;
                    }
                }
            }
        }
        tempo_map.build_timeline(division);
    }

    let file = File::open(path)?;
    let mut opt_decoder = Some(XzDecoder::new(BufReader::new(file)));

    let mut dummy_header = [0u8; 14];
    opt_decoder.as_mut().unwrap().read_exact(&mut dummy_header)?;

    let mut nps_buckets = vec![0u64; 28800]; 
    let mut total_notes = 0u64;
    let mut total_cc_events = 0u64;
    let mut tracks_count = 0;
    let mut chunk_tag = [0u8; 4];
    let mut max_calculated_sec = 0usize;

    loop {
        if opt_decoder.as_mut().unwrap().read_exact(&mut chunk_tag).is_err() { break; }

        if &chunk_tag == b"MTrk" {
            tracks_count += 1;
            let mut len_bytes = [0u8; 4];
            opt_decoder.as_mut().unwrap().read_exact(&mut len_bytes)?;
            let track_len = u32::from_be_bytes(len_bytes) as u64;

            let (bytes_tx, bytes_rx) = mpsc::sync_channel::<Vec<u8>>(64);
            let (events_tx, events_rx) = mpsc::sync_channel::<ParsedMidiEvent>(20000);

            let mut current_decoder = opt_decoder.take().unwrap();

            let reader_handle = thread::spawn(move || -> io::Result<XzDecoder<BufReader<File>>> {
                let mut track_bytes_left = track_len;
                let mut chunk = vec![0u8; 64 * 1024];
                while track_bytes_left > 0 {
                    let to_read = (chunk.len() as u64).min(track_bytes_left) as usize;
                    current_decoder.read_exact(&mut chunk[..to_read])?;
                    if bytes_tx.send(chunk[..to_read].to_vec()).is_err() { break; }
                    track_bytes_left -= to_read as u64;
                }
                Ok(current_decoder)
            });

            let parser_handle = thread::spawn(move || {
                let mut stream_reader = ChannelReader { rx: bytes_rx, current_chunk: Vec::new(), pos: 0, peeked_byte: None };
                let mut abs_tick = 0u32;
                let mut last_status = 0u8;
                let mut single_byte = [0u8; 1];

                loop {
                    let delta = match read_varint_from_stream(&mut stream_reader) {
                        Ok(d) => d,
                        Err(_) => break, 
                    };
                    abs_tick = abs_tick.saturating_add(delta);

                    if stream_reader.read_exact(&mut single_byte).is_err() { break; }
                    let mut status = single_byte[0];

                    if status >= 0x80 {
                        last_status = status;
                    } else {
                        status = last_status;
                        stream_reader.unget_byte(single_byte[0]);
                    }

                    let msg_type = status & 0xF0;
                    if msg_type == 0x90 {
                        let mut data = [0u8; 2];
                        if stream_reader.read_exact(&mut data).is_ok() {
                            let is_note_on = data[1] > 0;
                            let _ = events_tx.send(ParsedMidiEvent { abs_tick, is_note_on, is_cc: false });
                        }
                    } else if msg_type == 0x80 {
                        let mut data = [0u8; 2];
                        let _ = stream_reader.read_exact(&mut data);
                    } else if msg_type == 0xB0 {
                        let mut data = [0u8; 2];
                        if stream_reader.read_exact(&mut data).is_ok() {
                            let _ = events_tx.send(ParsedMidiEvent { abs_tick, is_note_on: false, is_cc: true });
                        }
                    } else if msg_type == 0xA0 || msg_type == 0xE0 {
                        let mut data = [0u8; 2];
                        let _ = stream_reader.read_exact(&mut data);
                    } else if msg_type == 0xC0 || msg_type == 0xD0 {
                        let _ = stream_reader.read_exact(&mut single_byte);
                    } else if status == 0xFF {
                        let _ = stream_reader.read_exact(&mut single_byte); 
                        if let Ok(meta_len) = read_varint_from_stream(&mut stream_reader) {
                            let mut dummy = vec![0u8; meta_len as usize];
                            let _ = stream_reader.read_exact(&mut dummy);
                        }
                    } else if status == 0xF0 || status == 0xF7 {
                        if let Ok(sys_len) = read_varint_from_stream(&mut stream_reader) {
                            let mut dummy = vec![0u8; sys_len as usize];
                            let _ = stream_reader.read_exact(&mut dummy);
                        }
                    }
                }
            });

            while let Ok(event) = events_rx.recv() {
                let sec = tempo_map.tick_to_secs(event.abs_tick, division) as usize;
                if sec < nps_buckets.len() {
                    if event.is_note_on {
                        total_notes += 1;
                        nps_buckets[sec] += 1;
                        if sec > max_calculated_sec { max_calculated_sec = sec; }
                    }
                }
                if event.is_cc { total_cc_events += 1; }
            }

            let recovered_decoder = reader_handle.join().unwrap()?;
            opt_decoder = Some(recovered_decoder);
            let _ = parser_handle.join();
        }
    }

    let max_nps = nps_buckets.into_iter().max().unwrap_or(0);
    let duration_secs = max_calculated_sec as f64;

    Ok(MidiExtendedInfo {
        filename: original_filename, file_size_mib, ppqn: division, tracks_count,
        total_notes, max_polyphony: 0, 
        max_nps, total_cc_events, duration_secs,
    })
}

fn main() {
    println!("=====================================================");
    println!("High-Performance MIDI Counter & Analyzer v1.0 Release");
    println!("=====================================================");
    println!("Please select a MIDI or XZ-compressed MIDI file...");

    let file_path = FileDialog::new()
        .add_filter("MIDI / XZ Compressed", &["mid", "midi", "xz"])
        .show_open_single_file();

    let path = match file_path {
        Ok(Some(p)) => p,
        _ => { println!("[-] No file selected. Exiting."); return; }
    };

    let filename = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let is_xz = path.extension().map_or(false, |ext| ext == "xz");
    
    println!("[+] Analyzing file, please wait...");
    let start_time = std::time::Instant::now();

    let result = if is_xz {
        extract_xz_parallel_stream(&path, filename)
    } else {
        extract_standard_midi_info(&path, filename)
    };

    match result {
        Ok(info) => {
            let elapsed = start_time.elapsed();
            println!("\n[+] Analysis successfully completed in {:.4?}\n", elapsed);
            
            println!("--- YOUTUBE TEMPLATE ---");
            println!("Filename: {}", info.filename);
            println!("Notes: {}", format_num(info.total_notes));
            println!("Max NPS: {}", format_num(info.max_nps));
            if info.max_polyphony > 0 {
                println!("Max Polyphony: {}", format_num(info.max_polyphony as u64));
            } else {
                println!("Max Polyphony: N/A (Stream Mode)");
            }
            println!("Total CC Events: {}", format_num(info.total_cc_events));
            println!("File Size: {:.2} MiB", info.file_size_mib);
            println!("Tracks Count: {}", info.tracks_count);
            println!("PPQ: {}", info.ppqn);
            if info.duration_secs > 0.0 {
                println!("Approx Duration: {}m {:.0}s", (info.duration_secs / 60.0) as u32, info.duration_secs % 60.0);
            }
            println!("-------------------------------------");
        }
        Err(e) => {
            println!("[-] Critical Error during processing: {}", e);
        }
    }

    println!("\nPress Enter to exit...");
    let mut buffer = String::new();
    let _ = io::stdin().read_line(&mut buffer);
}

fn format_num(num: u64) -> String {
    let num_str = num.to_string();
    let mut result = String::new();
    let mut count = 0;
    for c in num_str.chars().rev() {
        if count % 3 == 0 && count != 0 { result.push(','); }
        result.push(c);
        count += 1;
    }
    result.chars().rev().collect()
}
