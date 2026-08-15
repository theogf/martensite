// Parses the client's forwarded argument list.

#[derive(Debug, Clone)]
pub struct Switch {
    pub name: String,
    pub value: String,
}

#[derive(Debug)]
pub struct ParsedArgs {
    pub julia_channel: Option<String>,
    pub switches: Vec<Switch>,
    pub program_file: Option<String>,
    pub program_args: Vec<String>,
}

impl ParsedArgs {
    pub fn get_switch(&self, name: &str) -> Option<&str> {
        self.switches.iter().rev()
            .find(|s| s.name == name)
            .map(|s| s.value.as_str())
    }

    pub fn has_switch(&self, name: &str) -> bool {
        self.switches.iter().any(|s| s.name == name)
    }

    /// The effective `--threads`/`-t` spec, or `THREADS_NONE` if absent/empty.
    pub fn thread_switch(&self) -> Threads {
        parse_threads(self.get_switch("--threads").unwrap_or(""))
    }
}

static SHORT_TO_LONG: &[(&str, &str)] = &[
    ("-e", "--eval"),
    ("-E", "--print"),
    ("-L", "--load"),
    ("-P", "--project"),
    ("-t", "--threads"),
];

static NO_VALUE_SWITCHES: &[&str] = &[
    "-i", "-v", "--version", "-h", "--help",
    "--restart", "--sync", "--sandbox", "-q", "--quiet",
];

// Switches whose value may only be given as --switch=value; a following
// argument is the program file, as in `julia --project script.jl`.
static OPTIONAL_VALUE_SWITCHES: &[&str] = &[
    "--session",
    "--status",
    "--project",
    "--code-coverage",
    "--track-allocation",
    "--debug-info",
];

/// A Julia `--threads` spec as (default pool, interactive pool) counts.
///
/// Julia fixes thread counts at process startup, so this becomes part of a
/// worker's identity: a client can only reuse a worker spawned with the same
/// spec. Sentinels per field: `0` = unset (Julia default), `0xffff` = `auto`.
pub type Threads = [u16; 2];
pub const THREADS_UNSET: u16 = 0;
pub const THREADS_AUTO: u16 = 0xffff;
pub const THREADS_NONE: Threads = [THREADS_UNSET, THREADS_UNSET];

/// A single comparable value identifying a spec, for embedding in pool keys.
pub fn pack_threads(spec: Threads) -> u32 {
    ((spec[0] as u32) << 16) | spec[1] as u32
}

/// Parse a `--threads` value (`N`, `auto`, `N,M`, `auto,M`). Unrecognised
/// fields fall back to `auto`, leaving the final verdict to Julia at startup.
pub fn parse_threads(value: &str) -> Threads {
    if value.is_empty() { return THREADS_NONE; }
    let mut parts = value.splitn(2, ',');
    let default = parts.next().unwrap_or("");
    let interactive = parts.next().unwrap_or("");
    [parse_thread_field(default), parse_thread_field(interactive)]
}

fn parse_thread_field(field: &str) -> u16 {
    if field.is_empty() { return THREADS_UNSET; }
    if field == "auto" { return THREADS_AUTO; }
    field.parse().unwrap_or(THREADS_AUTO)
}

/// Render a spec as a `--threads` value (`3`, `auto`, `4,1`), or `None` when unset.
pub fn render_threads(spec: Threads) -> Option<String> {
    if spec[0] == THREADS_UNSET && spec[1] == THREADS_UNSET { return None; }
    let default = thread_field(spec[0]);
    Some(if spec[1] == THREADS_UNSET {
        default
    } else {
        format!("{},{}", default, thread_field(spec[1]))
    })
}

fn thread_field(val: u16) -> String {
    if val == THREADS_AUTO || val == THREADS_UNSET { "auto".to_string() } else { val.to_string() }
}

pub fn parse(args: &[String]) -> ParsedArgs {
    let mut switches = Vec::new();
    let mut julia_channel = None;
    let mut program_file = None;
    let mut seen_double_dash = false;
    let mut i = 1usize;

    // Check for JuliaUp channel selector (+1.10, +release, etc.)
    if let Some(first) = args.get(i) {
        if first.starts_with('+') {
            julia_channel = Some(first.clone());
            i += 1;
        }
    }

    while i < args.len() && program_file.is_none() {
        let arg = &args[i];
        i += 1;

        if arg == "--" {
            seen_double_dash = true;
        } else if seen_double_dash {
            program_file = Some(arg.clone());
        } else if arg.starts_with("--") {
            if let Some(eq) = arg.find('=') {
                switches.push(Switch { name: arg[..eq].to_string(), value: arg[eq+1..].to_string() });
            } else if NO_VALUE_SWITCHES.contains(&arg.as_str()) {
                switches.push(Switch { name: arg.clone(), value: String::new() });
            } else if OPTIONAL_VALUE_SWITCHES.contains(&arg.as_str()) {
                switches.push(Switch { name: arg.clone(), value: String::new() });
            } else {
                let value = args.get(i).cloned().unwrap_or_default();
                if args.get(i).is_some() { i += 1; }
                switches.push(Switch { name: arg.clone(), value });
            }
        } else if arg.len() > 1 && arg.starts_with('-') {
            let short = &arg[..2];
            let name = SHORT_TO_LONG.iter()
                .find(|(s, _)| *s == short)
                .map(|(_, l)| *l)
                .unwrap_or(short);
            if NO_VALUE_SWITCHES.contains(&name) {
                switches.push(Switch { name: name.to_string(), value: String::new() });
            } else {
                let value = if arg.len() > 2 {
                    // Accept both glued (`-t4`) and `=`-separated (`-t=4`) forms.
                    arg[2..].strip_prefix('=').unwrap_or(&arg[2..]).to_string()
                } else {
                    let v = args.get(i).cloned().unwrap_or_default();
                    if args.get(i).is_some() { i += 1; }
                    v
                };
                switches.push(Switch { name: name.to_string(), value });
            }
        } else {
            program_file = Some(arg.clone());
        }
    }

    let program_args = if program_file.is_some() {
        args[i..].to_vec()
    } else {
        Vec::new()
    };

    ParsedArgs { julia_channel, switches, program_file, program_args }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(args: &[&str]) -> ParsedArgs {
        // args[0] mirrors argv[0] (the program name), which parse() skips.
        let owned: Vec<String> = std::iter::once("julia".to_string())
            .chain(args.iter().map(|s| s.to_string()))
            .collect();
        parse(&owned)
    }

    #[test]
    fn short_flag_glued_value() {
        let parsed = parse_str(&["-t4"]);
        assert_eq!(parsed.get_switch("--threads"), Some("4"));
    }

    #[test]
    fn short_flag_equals_value() {
        // Regression test for the bug where `-t=4` silently became `auto`
        // because the leading `=` wasn't stripped before parsing the count.
        let parsed = parse_str(&["-t=4"]);
        assert_eq!(parsed.get_switch("--threads"), Some("4"));
        assert_eq!(parsed.thread_switch(), [4, THREADS_UNSET]);
    }

    #[test]
    fn short_flag_space_separated_value() {
        let parsed = parse_str(&["-t", "4"]);
        assert_eq!(parsed.get_switch("--threads"), Some("4"));
    }

    #[test]
    fn long_flag_equals_value() {
        let parsed = parse_str(&["--threads=4"]);
        assert_eq!(parsed.get_switch("--threads"), Some("4"));
    }

    #[test]
    fn short_flag_expands_to_long_name() {
        let parsed = parse_str(&["-P", "MyProject"]);
        assert!(parsed.has_switch("--project"));
        assert_eq!(parsed.get_switch("--project"), Some("MyProject"));
    }

    #[test]
    fn no_value_switch_takes_no_argument() {
        let parsed = parse_str(&["-i", "script.jl"]);
        assert_eq!(parsed.get_switch("-i"), Some(""));
        assert_eq!(parsed.program_file.as_deref(), Some("script.jl"));
    }

    #[test]
    fn optional_value_switch_without_equals_takes_no_argument() {
        // --session with no `=value` must not swallow the next arg as its value.
        let parsed = parse_str(&["--session", "script.jl"]);
        assert_eq!(parsed.get_switch("--session"), Some(""));
        assert_eq!(parsed.program_file.as_deref(), Some("script.jl"));
    }

    #[test]
    fn get_switch_returns_last_occurrence() {
        let parsed = parse_str(&["--threads=2", "--threads=4"]);
        assert_eq!(parsed.get_switch("--threads"), Some("4"));
    }

    #[test]
    fn julia_channel_prefix() {
        let parsed = parse_str(&["+1.10", "script.jl"]);
        assert_eq!(parsed.julia_channel.as_deref(), Some("+1.10"));
        assert_eq!(parsed.program_file.as_deref(), Some("script.jl"));
    }

    #[test]
    fn double_dash_marks_program_file() {
        let parsed = parse_str(&["--", "script.jl", "--not-a-switch"]);
        assert_eq!(parsed.program_file.as_deref(), Some("script.jl"));
        assert_eq!(parsed.program_args, vec!["--not-a-switch".to_string()]);
    }

    #[test]
    fn bare_positional_arg_is_program_file() {
        let parsed = parse_str(&["script.jl", "arg1", "arg2"]);
        assert_eq!(parsed.program_file.as_deref(), Some("script.jl"));
        assert_eq!(parsed.program_args, vec!["arg1".to_string(), "arg2".to_string()]);
    }

    #[test]
    fn thread_switch_defaults_to_none() {
        let parsed = parse_str(&["script.jl"]);
        assert_eq!(parsed.thread_switch(), THREADS_NONE);
    }

    #[test]
    fn parse_threads_variants() {
        assert_eq!(parse_threads(""), THREADS_NONE);
        assert_eq!(parse_threads("4"), [4, THREADS_UNSET]);
        assert_eq!(parse_threads("auto"), [THREADS_AUTO, THREADS_UNSET]);
        assert_eq!(parse_threads("4,1"), [4, 1]);
        assert_eq!(parse_threads("auto,2"), [THREADS_AUTO, 2]);
        // Unrecognised fields fall back to auto rather than erroring.
        assert_eq!(parse_threads("garbage"), [THREADS_AUTO, THREADS_UNSET]);
    }

    #[test]
    fn render_threads_round_trip() {
        assert_eq!(render_threads(THREADS_NONE), None);
        assert_eq!(render_threads([4, THREADS_UNSET]), Some("4".to_string()));
        assert_eq!(render_threads([THREADS_AUTO, THREADS_UNSET]), Some("auto".to_string()));
        assert_eq!(render_threads([4, 1]), Some("4,1".to_string()));
        assert_eq!(render_threads([THREADS_AUTO, 2]), Some("auto,2".to_string()));
    }

    #[test]
    fn pack_threads_is_order_sensitive_and_distinct() {
        let a = pack_threads([4, 1]);
        let b = pack_threads([1, 4]);
        assert_ne!(a, b);
        assert_eq!(pack_threads(THREADS_NONE), 0);
    }
}
