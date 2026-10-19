from pathlib import Path
p=Path('diagnostic-iocraft/packages/iocraft/src/terminal.rs')
s=p.read_text()
needle='''            match self.resize_cursor {'''
trace='''            if let Some(path) = std::env::var_os("IOCRAFT_GEOMETRY_LOG") {
                use std::io::Write as _;
                if let Ok(mut log) = std::fs::OpenOptions::new().append(true).create(true).open(path) {
                    let _ = writeln!(log, "pid={} prev_size={:?} size={:?} prev_height={} rows={} cursor={:?} offset={:?} pending={} previous={}", std::process::id(), self.prev_size_on_write, self.size, self.prev_canvas_height, rows, self.resize_cursor, self.inline_anchor.start_offset(), self.resize_pending, previous.map_or(String::new(), ToString::to_string));
                }
            }
'''
assert s.count(needle)==1
s=s.replace(needle,trace+needle)
needle='''                    let correction = projected.saturating_sub(retained);'''
trace='''                    if let Some(path) = std::env::var_os("IOCRAFT_GEOMETRY_LOG") {
                        use std::io::Write as _;
                        if let Ok(mut log) = std::fs::OpenOptions::new().append(true).create(true).open(path) {
                            let _ = writeln!(log, "pid={} retained={} projected={} correction={} row={} offset={}", std::process::id(), retained, projected, correction, row, offset);
                        }
                    }
'''
assert s.count(needle)==1
s=s.replace(needle,needle+'\n'+trace)
p.write_text(s)
p=Path('rust/crates/rusty-sudocode-cli/Cargo.toml')
s=p.read_text()
needle='iocraft = { git = "https://github.com/sudoprivacy/iocraft.git", rev = "e04d0675960b4df36399450af513b48a78a59f92" }'
assert needle in s
s=s.replace(needle,'iocraft = { path = "'+Path('diagnostic-iocraft/packages/iocraft').resolve().as_posix()+'" }')
p.write_text(s)
