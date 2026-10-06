//! Declarative argument validation shared by read-only command families.
//! Unknown options are serial, including abbreviations that the real program
//! might accept. `--` and short option values follow shell argv semantics.

pub(super) struct Flags {
    pub switches: &'static str,
    pub values: &'static str,
    pub attached: &'static str,
}

pub(super) struct Parsed<'a> {
    pub positional: Vec<&'a str>,
    pub flags: Vec<String>,
}

impl Flags {
    #[inline]
    fn contains(list: &str, flag: &str) -> bool {
        list.split_ascii_whitespace().any(|f| f == flag)
    }

    pub fn parse<'a>(&self, args: &'a [String]) -> Option<Parsed<'a>> {
        let mut parsed = Parsed {
            positional: Vec::new(),
            flags: Vec::new(),
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            if arg == "--" {
                parsed.positional.extend(args.map(String::as_str));
                break;
            }
            if arg == "-" || !arg.starts_with('-') {
                parsed.positional.push(arg);
                continue;
            }
            if arg.starts_with("--") {
                let (flag, value) = arg
                    .split_once('=')
                    .map_or((arg.as_str(), None), |(f, v)| (f, Some(v)));
                if Self::contains(self.values, flag) {
                    if value.is_none() {
                        args.next()?;
                    }
                } else if Self::contains(self.attached, flag) {
                    // Optional arguments must be attached. In particular,
                    // `git branch --abbrev 7` creates a branch named 7.
                } else if !Self::contains(self.switches, flag) || value.is_some() {
                    return None;
                }
                parsed.flags.push(flag.into());
            } else {
                for (offset, character) in arg[1..].char_indices() {
                    let flag = format!("-{character}");
                    if Self::contains(self.values, &flag) {
                        if offset + 1 + character.len_utf8() == arg.len() {
                            args.next()?;
                        }
                        parsed.flags.push(flag);
                        break;
                    }
                    if !Self::contains(self.switches, &flag) {
                        return None;
                    }
                    parsed.flags.push(flag);
                }
            }
        }
        Some(parsed)
    }
}
