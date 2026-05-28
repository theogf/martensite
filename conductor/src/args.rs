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
}

static SHORT_TO_LONG: &[(&str, &str)] = &[
    ("-e", "--eval"),
    ("-E", "--print"),
    ("-L", "--load"),
];

static NO_VALUE_SWITCHES: &[&str] = &[
    "-i", "-v", "--version", "-h", "--help",
    "--restart", "--sync", "--sandbox", "-q", "--quiet",
];

static OPTIONAL_VALUE_SWITCHES: &[&str] = &["--session"];

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
                    arg[2..].to_string()
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
