use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::LazyLock,
};

#[derive(Deserialize)]
pub struct Command {
    pub name: String,
    pub usage: String,
    pub summary: String,
    pub options: Vec<String>,
    pub details: String,
}

#[derive(Deserialize)]
struct Group {
    title: String,
    names: Vec<String>,
}

#[derive(Deserialize)]
pub struct Help {
    pub commands: Vec<Command>,
    common: Vec<String>,
    groups: Vec<Group>,
    sos: HashMap<String, String>,
}

const VERIFY_UNUSED_OPTIONS: &[&str] = &[
    "--dlls",
    "--smaps",
    "--cache",
    "--rebuild",
    "--native-roots",
];

pub static HELP: LazyLock<Help> =
    LazyLock::new(|| serde_json::from_str(include_str!("help.json")).expect("valid built-in help"));

pub struct Arguments {
    pub command: String,
    pub core: String,
    pub positional: Vec<String>,
    pub flags: HashSet<String>,
    pub dlls: Vec<String>,
    pub values: HashMap<String, String>,
}

impl Default for Arguments {
    fn default() -> Self {
        Self {
            command: "report".into(),
            core: String::new(),
            positional: Vec::new(),
            flags: HashSet::new(),
            dlls: Vec::new(),
            values: HashMap::new(),
        }
    }
}

impl Arguments {
    pub fn value(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }
    pub fn flag(&self, key: &str) -> bool {
        self.flags.contains(key)
    }
    pub fn limit(&self) -> usize {
        self.value("--limit")
            .and_then(|n| n.parse::<i64>().ok())
            .map(|n| if n > 0 { n as usize } else { usize::MAX })
            .unwrap_or(20)
    }
}

pub enum Parsed {
    Run(Arguments),
    Print(String, u8),
}

fn command(name: &str) -> Option<&'static Command> {
    HELP.commands.iter().find(|c| c.name == name)
}

fn is_command(name: &str) -> bool {
    name == "report" || command(name).is_some()
}

pub fn help(name: Option<&str>) -> String {
    if name == Some("report") {
        return format!(
            r#"usage: dotheap report <core> [options]
       dotheap <core> [options]

Full text report of heap, native memory, ownership and diagnostics.
Use individual commands with --json for structured output.

options:
  --top <n>          rows per report table (default 30)
  --explain <type>   inspect the largest live object whose type contains this text
  --diagnostics      include additional heap diagnostics

analysis options:
{}"#,
            option_lines(&HELP.common[2..])
        );
    }
    let Some(c) = name.and_then(command) else {
        return overview();
    };
    let usage = if c.usage.is_empty() {
        String::new()
    } else {
        format!(" {}", c.usage)
    };
    let mut text = format!(
        "usage: dotheap {}{usage} [options]\n\n{}\n",
        c.name, c.summary
    );
    if !c.options.is_empty() {
        text += &format!("\noptions:\n{}\n", option_lines(&c.options));
    }
    if !c.details.is_empty() {
        text += &format!("\n{}\n", c.details);
    }
    let common = match c.name.as_str() {
        "dump" => {
            vec!["--json            print the path as {\"dump\":\"<path>\"} on stdout".into()]
        }
        "ps" => HELP.common[..1].to_vec(),
        "cache" => HELP.common[..2].to_vec(),
        "verify" => HELP
            .common
            .iter()
            .filter(|line| !VERIFY_UNUSED_OPTIONS.iter().any(|o| line.starts_with(o)))
            .cloned()
            .collect(),
        _ => HELP.common.clone(),
    };
    text += &format!("\ncommon options:\n{}", option_lines(&common));
    text
}

fn option_lines(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn overview() -> String {
    let groups = HELP
        .groups
        .iter()
        .map(|group| {
            let commands = group
                .names
                .iter()
                .filter_map(|name| command(name))
                .map(|c| format!("    {:18}{}", c.name, c.summary))
                .collect::<Vec<_>>()
                .join("\n");
            format!("  {}:\n{commands}", group.title)
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"usage: dotheap <command> <core> [arguments] [options]
       dotheap <core> [--top <n>] [--explain <type>] [--diagnostics]   (the full text report, also `dotheap report`)

Analyze managed and native memory in a .NET process dump.
Supports Linux x64/arm64 and macOS arm64 dumps, with CoreCLR or Satori GC.
The first analysis processes the dump and caches results in the user cache.
Later commands reuse them; use --progress to see processing phases.

Start with `summary` and follow its `next` commands. Use `object`, `retained`,
`regions` or `chunk` to inspect an address. Run `dotheap help <command>` for
arguments, interpretation and limitations.

For scripts and agents, use --json and check the exit status: 0 success,
1 failure, 2 invalid arguments. Results go to stdout; progress and warnings
go to stderr. JSON lists shortened by --limit have a sibling <name>Omitted
count. Text tables can shorten cells over 80 characters; JSON has no table-cell limit.

commands:
{groups}
  other:
    report            the full text report (--top <n>, --explain <type>, --diagnostics)
    help <command>
    --version         the version of dotheap

analysis options:
{}"#,
        option_lines(&HELP.common)
    )
}

fn unknown(name: &str) -> String {
    if let Some(hint) = HELP.sos.get(&name.to_lowercase()) {
        format!("unknown command {name} (an SOS command); {hint}")
    } else {
        format!("unknown command {name}; see dotheap help")
    }
}

fn option_names(lines: &[String]) -> HashSet<&str> {
    lines
        .iter()
        .flat_map(|line| {
            line.split("  ")
                .next()
                .unwrap_or("")
                .split([' ', ','])
                .filter(|word| word.starts_with("--"))
        })
        .collect()
}

pub fn parse(raw: &[String]) -> Result<Parsed, String> {
    let Some(first) = raw.first() else {
        return Ok(Parsed::Print(overview(), 2));
    };
    if raw.iter().any(|s| s == "--version") {
        return Ok(Parsed::Print(env!("CARGO_PKG_VERSION").into(), 0));
    }
    if ["help", "--help", "-h"].contains(&first.as_str()) {
        let name = raw.get(1).map(String::as_str);
        if let Some(name) = name
            && !is_command(name)
        {
            return Err(unknown(name));
        }
        return Ok(Parsed::Print(help(name), 0));
    }
    let mut a = Arguments::default();
    let named = is_command(first);
    let list = if named {
        a.command = first.clone();
        &raw[1..]
    } else {
        if !first.starts_with('-')
            && !Path::new(first).exists()
            && (HELP.sos.contains_key(&first.to_lowercase()) || !first.contains(['/', '.']))
        {
            return Err(if HELP.sos.contains_key(&first.to_lowercase()) {
                unknown(first)
            } else {
                format!("{first} is neither a command nor a file; see dotheap help")
            });
        }
        raw
    };
    let value_options = [
        "--out",
        "--pid",
        "--delete",
        "--count",
        "--dlls",
        "--smaps",
        "--cache",
        "--limit",
        "--sort",
        "--match",
        "--text",
        "--reach",
        "--library",
        "--group",
        "--size",
        "--by",
        "--top",
        "--explain",
    ];
    let flag_options = [
        "--json",
        "--no-cache",
        "--rebuild",
        "--native-roots",
        "--progress",
        "--live",
        "--pending",
        "--dead",
        "--diagnostics",
        "--offline",
    ];
    let mut given = Vec::new();
    let mut k = 0;
    while k < list.len() {
        let arg = if list[k] == "-o" { "--out" } else { &list[k] };
        if ["--help", "-h"].contains(&arg) {
            return Ok(Parsed::Print(help(Some(&a.command)), 0));
        }
        if value_options.contains(&arg) {
            let value = list
                .get(k + 1)
                .ok_or_else(|| format!("{arg} needs a value"))?;
            if arg == "--dlls" {
                a.dlls.push(value.clone());
            } else {
                a.values.insert(arg.into(), value.clone());
            }
            given.push(arg);
            k += 2;
        } else if flag_options.contains(&arg) {
            a.flags.insert(arg.into());
            given.push(arg);
            k += 1;
        } else if arg.starts_with('-') && arg.len() > 1 && arg.parse::<i64>().is_err() {
            given.push(arg);
            k += 1;
        } else {
            if a.core.is_empty() {
                a.core = arg.into();
            } else {
                a.positional.push(arg.into());
            }
            k += 1;
        }
    }
    if !named && let Some(next) = a.positional.first() {
        if is_command(next) {
            a.command = a.positional.remove(0);
        } else if HELP.sos.contains_key(&next.to_lowercase()) {
            return Err(unknown(next));
        }
    }
    let c = command(&a.command);
    let mut allowed = option_names(&HELP.common);
    allowed.extend(["--offline", "--out"]);
    if let Some(c) = c {
        allowed.extend(option_names(&c.options));
    } else {
        allowed.remove("--json");
        allowed.remove("--limit");
        allowed.extend(["--top", "--explain", "--diagnostics"]);
    }
    if a.command == "verify" {
        allowed.retain(|option| !VERIFY_UNUSED_OPTIONS.contains(option));
    }
    if let Some(option) = given.into_iter().find(|option| !allowed.contains(option)) {
        let dashes = format!("--{}", option.trim_start_matches('-'));
        let hint = match option {
            "--json" if c.is_none() => {
                "the report is text only; the other commands take --json".into()
            }
            "--limit" | "-n" if c.is_none() => "use --top <n>".into(),
            "-n" | "--top" => "use --limit <n>".into(),
            "--format" => "use --json".into(),
            "--output" => "use --out <path>".into(),
            _ if allowed.contains(dashes.as_str()) => format!("use {dashes}"),
            _ => format!("see dotheap help {}", a.command),
        };
        return Err(format!("unknown option {option} for {}; {hint}", a.command));
    }
    for option in ["--limit", "--top", "--count"] {
        if let Some(value) = a.value(option)
            && !value.parse::<i64>().is_ok_and(|n| n >= 0)
        {
            return Err(format!("{option} needs a non-negative integer"));
        }
    }
    if let Some(size) = a.value("--size") {
        if size.parse::<u64>().is_err() {
            return Err("--size needs a chunk size in bytes".into());
        }
        if a.value("--group").is_none() {
            return Err("--size requires --group; use native to list group IDs".into());
        }
    }
    let usage = c.map(|c| c.usage.as_str()).unwrap_or("<core>");
    let extra = usage.matches('<').count().saturating_sub(1);
    let no_core = ["ps", "cache"].contains(&a.command.as_str());
    let most = if no_core {
        0
    } else if a.value("--pid").is_some() {
        extra
    } else {
        extra + 1
    };
    let supplied: Vec<_> = (!a.core.is_empty())
        .then_some(&a.core)
        .into_iter()
        .chain(&a.positional)
        .collect();
    if supplied.len() > most {
        let hint = if usage.is_empty() {
            format!("dotheap {} takes no arguments", a.command)
        } else {
            format!("usage: dotheap {} {usage}", a.command)
        };
        return Err(format!("unexpected argument {}; {hint}", supplied[most]));
    }
    if a.core.is_empty() && !no_core && a.value("--pid").is_none() {
        return Err(format!(
            "missing <{}>; see dotheap help {}",
            if a.command == "dump" { "pid" } else { "core" },
            a.command
        ));
    }
    Ok(Parsed::Run(a))
}
