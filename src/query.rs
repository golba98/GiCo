//! Small compositional grammar for common file queries. Every production consumes
//! the whole request. Literals retain their spelling and are quoted on rendering.
use crate::context::fs::shell_quote;
use regex_lite::{Captures, Regex};
use std::{path::Path, sync::LazyLock};

macro_rules! captures {
    ($pattern:literal, $text:expr) => {{
        static RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(concat!("(?i)^(?:", $pattern, ")$")).expect("constant grammar")
        });
        RE.captures($text)
    }};
}

#[derive(Debug)]
pub(crate) enum Transform {
    Extension,
    Contents,
    JsonKey(String),
    Count(&'static str),
    Slice {
        end: bool,
        amount: usize,
        unit: String,
    },
    Line(usize),
    Field {
        index: usize,
        separator: char,
    },
    Sort {
        numeric: bool,
        reverse: bool,
        unique: bool,
    },
    Extreme {
        highest: bool,
    },
    Match {
        pattern: String,
        regex: bool,
        exclude: bool,
        output: MatchOutput,
    },
    DistinctCount,
    SortField {
        index: usize,
        separator: char,
        numeric: bool,
        reverse: bool,
    },
    Nonempty,
}

#[derive(Debug, PartialEq)]
pub(crate) enum MatchOutput {
    Lines,
    Numbers,
    Count,
    Files,
}

#[derive(Debug)]
pub(crate) enum Query {
    Write {
        inputs: Vec<String>,
        destination: String,
        method: Write,
    },
    Launch {
        input: String,
        python: bool,
    },

    DirectoryName,
    Architecture,
    UpgradePackages,
    Journal {
        severity: String,
        limit: usize,
        unit: Option<String>,
    },
    DiskSpace,
    TreeSize,
    Ranked {
        scope: String,
        limit: usize,
        largest: bool,
    },
    AggregateLines(String),
    Stream {
        input: String,
        operation: Transform,
        destination: Option<Destination>,
    },
    Search {
        scope: String,
        shallow: bool,
        predicate: Search,
        exclude: Option<String>,
    },
}

#[derive(Debug)]
pub(crate) struct Destination {
    path: String,
    overwrite: bool,
}

#[derive(Debug)]
pub(crate) enum Write {
    Copy,
    Text(String),
    Join,
    Append(String),
    Empty,
}

#[derive(Debug)]
pub(crate) enum Search {
    Name(String),
    Empty,
    Link,
    Larger(usize),
    Content(String, bool),
}

fn literal(s: &str) -> String {
    s.replace('\u{1f}', " ")
}

fn number(s: &str) -> Option<usize> {
    let n = [
        "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    ]
    .iter()
    .position(|w| s.eq_ignore_ascii_case(w))
    .or_else(|| {
        [
            "first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth", "ninth",
            "tenth",
        ]
        .iter()
        .position(|w| s.eq_ignore_ascii_case(w))
    })
    .map(|i| i + 1)
    .or_else(|| s.parse().ok())?;
    (n > 0 && n <= 1_000_000).then_some(n)
}

fn capture(c: &Captures<'_>, name: &str) -> Option<String> {
    c.name(name).map(|m| literal(m.as_str()))
}

impl Query {
    pub(crate) fn parse(request: &str) -> Option<Self> {
        if request.chars().any(char::is_control) || request.len() > 4096 {
            return None;
        }
        let tokens = shlex::split(request.trim().trim_end_matches('?'))?;
        let sentence = tokens
            .iter()
            .map(|t| t.replace(' ', "\u{1f}"))
            .collect::<Vec<_>>()
            .join(" ");
        let text = sentence
            .trim_end_matches(", print the number only")
            .trim_end_matches(" and print just the number");
        let (text, overwrite) = text
            .strip_suffix(" and overwrite the destination")
            .or_else(|| text.strip_suffix(", overwriting the destination"))
            .map_or((text, false), |base| (base, true));
        if overwrite && !text.starts_with("write ") {
            return None;
        }
        let text = text
            .trim_start_matches("please ")
            .trim_start_matches("Please ");
        let body = captures!(r"(?:show|give me|print|display|read|list) (.+)", text)
            .and_then(|c| c.get(1))
            .map_or(text, |m| m.as_str());
        let body = body
            .trim_start_matches("just ")
            .trim_start_matches("only ")
            .trim_start_matches("the ");
        if captures!(
            r"(?:update|upgrade) (?:all |the )?(?:installed |system )?packages",
            text
        )
        .is_some()
        {
            return Some(Self::UpgradePackages);
        }
        if let Some(c) = captures!(
            r"(?:most recent |recent |latest |last )(?:(?P<n>[0-9]+) )?(?:system |journal )?(?P<priority>errors|warnings)(?: (?:from|for) (?:the )?(?P<unit>[a-z0-9@_.-]+))?",
            body
        ) {
            let limit = if let Some(n) = c.name("n") {
                number(n.as_str())?
            } else {
                20
            };
            return Some(Self::Journal {
                severity: if c["priority"].eq_ignore_ascii_case("errors") {
                    "err".into()
                } else {
                    "warning".into()
                },
                limit,
                unit: capture(&c, "unit"),
            });
        }
        if captures!(r"(?:machine|system|processor|cpu) architecture", body).is_some() {
            return Some(Self::Architecture);
        }
        if let Some(c) = captures!(
            r"(?:create|write) (?:a file (?:called|named) )?([^ ]+) (?:containing|with) (?:only )?([^ ]+)(?: on (?:its own|one) line)?",
            text
        ) {
            return Some(Self::Write {
                inputs: Vec::new(),
                destination: literal(&c[1]),
                method: Write::Text(literal(&c[2])),
            });
        }
        if let Some(c) = captures!(
            r"(?P<n>[^ ]+) (?P<order>largest|biggest|smallest) (?:regular )?files(?: (?:under|within|in) (?P<scope>[^ ]+))?",
            body
        ) {
            return Some(Self::Ranked {
                scope: capture(&c, "scope").unwrap_or_else(|| ".".into()),
                limit: number(&c["n"])?,
                largest: !c["order"].eq_ignore_ascii_case("smallest"),
            });
        }
        if let Some(c) = captures!(
            r"count (?:the )?(?:total )?lines (?:in|of) all ([a-z0-9.]+) files",
            body
        ) {
            let ext = c[1].trim_start_matches('.');
            return Some(Self::AggregateLines(format!("*.{ext}")));
        }
        if captures!(r"(?:basename|base name|name) of (?:the )?(?:current|present)(?: working)? (?:directory|folder)|(?:current|present)(?: working)? directory basename",body).is_some(){return Some(Self::DirectoryName);}
        if captures!(r"(?:filesystem |disk )?(?:free|available) space (?:for this directory|here|in this directory)",body).is_some(){return Some(Self::DiskSpace);}
        if captures!(r"(?:how much (?:disk )?space does (?:this|the current) (?:directory|folder) (?:use|occupy)|size of (?:this|the current) (?:directory|folder))",body).is_some(){return Some(Self::TreeSize);}
        if let Some(c) = captures!(r"(?:copy|duplicate) ([^ ]+) (?:to|as) ([^ ]+)", text) {
            return Some(Self::Write {
                inputs: vec![literal(&c[1])],
                destination: literal(&c[2]),
                method: Write::Copy,
            });
        }
        if let Some(c) = captures!(
            r"(?:combine|concatenate|join) ([^ ]+) and ([^ ]+)(?: in that order)? (?:into|to) ([^ ]+)",
            text
        ) {
            return Some(Self::Write {
                inputs: vec![literal(&c[1]), literal(&c[2])],
                destination: literal(&c[3]),
                method: Write::Join,
            });
        }
        if let Some(c) = captures!(r"append ([^ ]+)(?: on a new line)? to ([^ ]+)", text) {
            return Some(Self::Write {
                inputs: vec![],
                destination: literal(&c[2]),
                method: Write::Append(literal(&c[1])),
            });
        }
        if let Some(c) = captures!(
            r"(?:create|make) (?:an? )?empty file (?:named |called )?([^ ]+)",
            text
        ) {
            return Some(Self::Write {
                inputs: vec![],
                destination: literal(&c[1]),
                method: Write::Empty,
            });
        }
        if let Some(c) = captures!(
            r"(?:run|execute|start|launch) (?:the )?(?P<kind>python script|service executable|executable|script) (?P<file>[^ ]+)(?: (?:using|with) (?P<runtime>python))?",
            text
        ) {
            let python =
                c["kind"].eq_ignore_ascii_case("python script") || c.name("runtime").is_some();
            return Some(Self::Launch {
                input: literal(&c["file"]),
                python,
            });
        }
        let stream = |c: &Captures<'_>, operation| {
            Some(Self::Stream {
                input: capture(c, "file")?,
                operation,
                destination: capture(c, "dest").map(|path| Destination { path, overwrite }),
            })
        };
        if let Some(c) = captures!(
            r"sort (?P<file>[^ ]+)(?: (?P<mode>numerically|lexically))? by (?:the )?(?P<n>[^ ]+) (?P<sep>tab|comma|colon)-separated (?:field|column)(?: (?:in )?(?P<dir>descending|ascending)(?: order)?)?",
            body
        ) {
            let separator = match c["sep"].to_ascii_lowercase().as_str() {
                "tab" => '\t',
                "comma" => ',',
                _ => ':',
            };
            return stream(
                &c,
                Transform::SortField {
                    index: number(&c["n"])?,
                    separator,
                    numeric: c
                        .name("mode")
                        .is_some_and(|v| v.as_str().eq_ignore_ascii_case("numerically")),
                    reverse: c
                        .name("dir")
                        .is_some_and(|v| v.as_str().eq_ignore_ascii_case("descending")),
                },
            );
        }
        if let Some(c) = captures!(r"(?:contents|content) of (?P<file>[^ ]+)", body) {
            return stream(&c, Transform::Contents);
        }
        if let Some(c) = captures!(r"(?:file )?extension of (?P<file>[^ ]+)", body) {
            return stream(&c, Transform::Extension);
        }
        if let Some(c) = captures!(
            r"(?:value of|json field) (?P<key>[a-z_][a-z_0-9]*) (?:in|from) (?P<file>[^ ]+)",
            body
        ) {
            return stream(&c, Transform::JsonKey(literal(&c["key"])));
        }
        if let Some(c) = captures!(
            r"(?:(?:number|count) of |count (?:the )?|how many )?(?P<unit>lines|bytes|words|characters)(?: are)? (?:in|from|of) (?P<file>[^ ]+)",
            body
        ) {
            // "lines from file" asks for the contents, not a count.
            if !text
                .to_ascii_lowercase()
                .split_whitespace()
                .any(|w| matches!(w, "number" | "count" | "many"))
            {
                return None;
            }
            let flag = match c["unit"].to_ascii_lowercase().as_str() {
                "words" => "-w",
                "bytes" => "-c",
                "characters" => "-m",
                _ => "-l",
            };
            return stream(&c, Transform::Count(flag));
        }
        if let Some(c) = captures!(
            r"(?:(?:number|count) of |count )(?:distinct|unique) (?:values|lines|rows) (?:in|of|from) (?P<file>[^ ]+)",
            body
        ) {
            return stream(&c, Transform::DistinctCount);
        }
        if let Some(c) = captures!(
            r"(?P<end>first|last|final)(?: (?P<n>[^ ]+))? (?P<unit>lines?|bytes?|characters?) (?:in|of|from) (?:the symlink )?(?P<file>[^ ]+)",
            body
        ) {
            let amount = if let Some(n) = c.name("n") {
                number(n.as_str())?
            } else {
                1
            };
            return stream(
                &c,
                Transform::Slice {
                    end: !c["end"].eq_ignore_ascii_case("first"),
                    amount,
                    unit: c["unit"].to_ascii_lowercase(),
                },
            );
        }
        if let Some(c) = captures!(r"(?P<n>[^ ]+) line (?:in|of|from) (?P<file>[^ ]+)", body) {
            return stream(&c, Transform::Line(number(&c["n"])?));
        }
        if let Some(c) = captures!(
            r"(?P<n>[^ ]+) (?P<sep>comma|tab|colon|pipe)[- ](?:separated|delimited) (?:field|column) (?:of|in|from) (?P<file>[^ ]+)",
            body
        ) {
            let separator = match c["sep"].to_ascii_lowercase().as_str() {
                "comma" => ',',
                "tab" => '\t',
                "colon" => ':',
                _ => '|',
            };
            return stream(
                &c,
                Transform::Field {
                    index: number(&c["n"])?,
                    separator,
                },
            );
        }
        if let Some(c) = captures!(
            r"(?:all )?(?:nonempty|non-empty) (?:lines|rows) (?:from|in|of) (?P<file>[^ ]+)",
            body
        ) {
            return stream(&c, Transform::Nonempty);
        }
        if let Some(c) = captures!(
            r"(?P<mode>highest|largest|smallest|lowest|maximum|minimum) (?:integer|number|value) (?:from|in|of) (?P<file>[^ ]+)",
            body
        ) {
            return stream(
                &c,
                Transform::Extreme {
                    highest: matches!(
                        c["mode"].to_ascii_lowercase().as_str(),
                        "highest" | "largest" | "maximum"
                    ),
                },
            );
        }
        if let Some(c) = captures!(
            r"(?:sort |write (?:the )?(?P<uniq>unique )?sorted (?:contents|lines) (?:from|of) |(?P<unique>unique lines in )?)(?P<file>[^ ]+)(?: (?:in )?(?P<order>(?:reverse )?alphabetical order|numerically|lexically|increasing|descending|ascending))?(?: (?:in |from )?(?P<direction>descending order|ascending order|smallest to largest|largest to smallest))?(?: (?:to|into) (?P<dest>[^ ]+))?",
            body
        ) {
            if body.starts_with("sort ")
                || body.starts_with("write ")
                || c.name("order").is_some()
                || c.name("unique").is_some()
            {
                let order = capture(&c, "order").unwrap_or_default().to_lowercase();
                let dir = capture(&c, "direction").unwrap_or_default().to_lowercase();
                return stream(
                    &c,
                    Transform::Sort {
                        numeric: order == "numerically",
                        reverse: order.starts_with("reverse")
                            || order == "descending"
                            || dir.starts_with("descending")
                            || dir.starts_with("largest"),
                        unique: c.name("uniq").or(c.name("unique")).is_some(),
                    },
                );
            }
        }
        if let Some(c) = captures!(
            r"(?P<count>count )?(?:all |only )?(?P<kind>matching line number|line numbers|lines|rows)(?: (?:from|in) (?P<src>[^ ]+))? (?:that )?(?P<verb>do not contain|not containing|excluding|containing|contain|matching|for) (?:the )?(?:(?P<mode>literal|regex) )?(?P<pattern>[^ ]+?)(?: (?:in|from) (?P<file>[^ ,]+))?(?:, just the number)?",
            body
        ) {
            let input = capture(&c, "file").or_else(|| capture(&c, "src"))?;
            if c.name("file").is_some() && c.name("src").is_some() {
                return None;
            }
            let output = if c.name("count").is_some() {
                MatchOutput::Count
            } else if c["kind"].contains("number") {
                MatchOutput::Numbers
            } else {
                MatchOutput::Lines
            };
            return Some(Self::Stream {
                input,
                operation: Transform::Match {
                    pattern: capture(&c, "pattern")?,
                    regex: c.name("mode").is_some_and(|m| m.as_str() == "regex"),
                    exclude: matches!(
                        &c["verb"],
                        "excluding" | "not containing" | "do not contain"
                    ),
                    output,
                },
                destination: None,
            });
        }
        Self::search(text)
    }

    fn search(text: &str) -> Option<Self> {
        let mut body = captures!(r"(?:find|list|which|show|locate) (.+)", text)?
            .get(1)?
            .as_str();
        body = body
            .trim_start_matches("just ")
            .trim_start_matches("only ")
            .trim_start_matches("all ")
            .trim_start_matches("the ");
        body = body
            .trim_end_matches(", print one filename per line")
            .trim_end_matches(" recursively");
        let mut exclude = None;
        if let Some((main, skip)) = body.rsplit_once(" except ") {
            if skip.contains(' ') {
                return None;
            }
            exclude = Some(literal(skip));
            body = main;
        }
        let mut scope = ".".to_string();
        let shallow = body.ends_with("in the current directory")
            || body.ends_with("in the current directory only");
        if shallow {
            body = body.split_once(" in the current directory")?.0;
        } else if let Some(c) = captures!(r"(.+) (?:under|below|within) ([^ ]+)(?: only)?", body) {
            body = c.get(1)?.as_str();
            scope = literal(&c[2]);
        }
        let predicate = if let Some(c) = captures!(
            r"(?:files|names|filenames) (?:named|ending in) ([^ ]+)(?: exactly)?",
            body
        ) {
            let name = literal(&c[1]);
            if body.contains("named") && name.contains(['*', '?', '[']) {
                return None;
            }
            Search::Name(if body.contains("ending in") {
                format!("*{name}")
            } else {
                name
            })
        } else if let Some(c) = captures!(r"([^ ]+) files", body) {
            let ext = c[1]
                .trim_start_matches('*')
                .trim_start_matches('.')
                .to_ascii_lowercase();
            if ext == "empty" {
                Search::Empty
            } else {
                let ext = match ext.as_str() {
                    "markdown" => "md",
                    "python" => "py",
                    "rust" => "rs",
                    "typescript" => "ts",
                    "javascript" => "js",
                    _ if c[1].starts_with('.') || c[1].starts_with("*.") => &ext,
                    _ => return None,
                };
                Search::Name(format!("*.{ext}"))
            }
        } else if body == "empty regular files" {
            Search::Empty
        } else if matches!(body, "symbolic links" | "symlinks") {
            Search::Link
        } else if let Some(c) = captures!(r"files (?:larger|bigger) than ([0-9]+) bytes", body) {
            Search::Larger(c[1].parse().ok()?)
        } else if let Some(c) = captures!(
            r"files (?:that )?(?:contain|containing|matching) (?:the )?(?:(regex|literal) )?([^ ]+)",
            body
        ) {
            Search::Content(
                literal(&c[2]),
                c.get(1).is_some_and(|m| m.as_str() == "regex"),
            )
        } else {
            return None;
        };
        Some(Self::Search {
            scope,
            shallow,
            predicate,
            exclude,
        })
    }

    pub(crate) fn render(&self, cwd: &Path) -> Result<String, String> {
        let path = |s: &str| {
            shell_quote(&if s.starts_with('-') {
                format!("./{s}")
            } else {
                s.to_owned()
            })
        };
        match self {
            Self::UpgradePackages => {
                use crate::context::env::on_path;
                // The distro's own metadata takes precedence when multiple managers coexist.
                let distro = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
                let family = distro
                    .lines()
                    .filter(|l| l.starts_with("ID=") || l.starts_with("ID_LIKE="))
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_lowercase();
                let cmd = if ["fedora", "rhel", "centos"]
                    .iter()
                    .any(|id| family.contains(id))
                    && on_path("dnf")
                {
                    "sudo dnf upgrade"
                } else if ["debian", "ubuntu"].iter().any(|id| family.contains(id))
                    && on_path("apt-get")
                {
                    "sudo apt-get update && sudo apt-get upgrade"
                } else if family.contains("arch") && on_path("pacman") {
                    "sudo pacman -Syu"
                } else if family.contains("suse") && on_path("zypper") {
                    "sudo zypper update"
                } else {
                    return Err("Which system package manager should handle the upgrade?".into());
                };
                Ok(cmd.into())
            }
            Self::Journal {
                severity,
                limit,
                unit,
            } => {
                let mut command = format!("journalctl --no-pager -p {severity} -n {limit}");
                if let Some(u) = unit {
                    command.push_str(&format!(" -u {}", shell_quote(u)));
                }
                Ok(command)
            }
            Self::Architecture => Ok("uname -m".into()),
            Self::DirectoryName => Ok("basename -- \"$PWD\"".into()),
            Self::DiskSpace => Ok("df -h .".into()),
            Self::Ranked {
                scope,
                limit,
                largest,
            } => {
                if !cwd.join(scope).is_dir() {
                    return Err(format!("Search directory does not exist: {scope}"));
                }
                Ok(format!("find {} -type f -printf '%s %p\\0' | sort -z {} | head -z -n {limit} | tr '\\0' '\\n'",path(scope),if *largest{"-nr"}else{"-n"}))
            }
            Self::AggregateLines(p) => Ok(format!(
                "find . -type f -name {} -exec cat -- '{{}}' + | wc -l",
                shell_quote(p)
            )),
            Self::TreeSize => Ok("du -sh .".into()),
            Self::Launch { input, python } => {
                if !cwd.join(input).is_file() {
                    return Err(format!("Executable does not exist: {input}"));
                }
                if *python {
                    return Ok(format!("python3 -- {}", path(input)));
                }
                use std::os::unix::fs::PermissionsExt;
                if !cwd
                    .join(input)
                    .metadata()
                    .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
                {
                    return Err(format!("File is not executable: {input}"));
                }
                Ok(
                    if Path::new(input).is_absolute() || input.starts_with("./") {
                        path(input)
                    } else {
                        path(&format!("./{input}"))
                    },
                )
            }
            Self::Write {
                inputs,
                destination,
                method,
            } => {
                if let Some(f) = inputs.iter().find(|f| !cwd.join(f).is_file()) {
                    return Err(format!("Input file does not exist: {f}"));
                }
                let d = cwd.join(destination);
                if !d.parent().is_some_and(Path::is_dir) {
                    return Err(format!(
                        "Destination directory does not exist: {destination}"
                    ));
                }
                if let Ok(m) = d.symlink_metadata() {
                    if !matches!(method, Write::Append(_)) || !m.is_file() {
                        return Err(format!("Destination {destination} already exists. Specify a new path or whether to overwrite it."));
                    }
                }
                let out = path(destination);
                Ok(match method {
                    Write::Text(value) => format!("printf '%s\\n' {} > {out}", shell_quote(value)),
                    Write::Empty => format!("touch -- {out}"),
                    Write::Copy => format!("cp -- {} {out}", path(&inputs[0])),
                    Write::Join => {
                        format!("cat -- {} {} > {out}", path(&inputs[0]), path(&inputs[1]))
                    }
                    Write::Append(value) => {
                        format!("printf '%s\\n' {} >> {out}", shell_quote(value))
                    }
                })
            }
            Self::Stream {
                input,
                operation,
                destination,
            } => {
                if !cwd.join(input).is_file() {
                    return Err(format!("Input file does not exist: {input}"));
                }
                let f = path(input);
                let mut cmd = match operation {
                    Transform::Contents => format!("cat -- {f}"),
                    Transform::Extension => format!("basename -- {f} | awk -F . '{{print $NF}}'"),
                    Transform::JsonKey(key) => {
                        format!("jq -r --arg key {} '.[$key]' -- {f}", shell_quote(key))
                    }
                    Transform::Count(flag) => format!("wc {flag} < {f}"),
                    Transform::DistinctCount => format!("sort -u -- {f} | wc -l"),
                    Transform::Slice { end, amount, unit } => {
                        if unit.starts_with("character") {
                            // POSIX cut works on each line. Keep character selection on
                            // the full stream so UTF-8 and embedded newlines are counted.
                            let script = if *end {
                                format!("import sys, collections; sys.stdout.write(\"\".join(collections.deque(iter(lambda: sys.stdin.read(1), \"\"), maxlen={amount})))")
                            } else {
                                format!("import sys; sys.stdout.write(sys.stdin.read({amount}))")
                            };
                            format!("python3 -c {} < {f}", shell_quote(&script))
                        } else {
                            format!(
                                "{} {} {amount} -- {f}",
                                if *end { "tail" } else { "head" },
                                if unit.starts_with("byte") { "-c" } else { "-n" }
                            )
                        }
                    }
                    Transform::Line(n) => format!("sed -n '{n}p' -- {f}"),
                    Transform::Field { index, separator } => {
                        if *separator == '\t' {
                            format!("cut -f {index} -- {f}")
                        } else {
                            format!(
                                "cut -d {} -f {index} -- {f}",
                                shell_quote(&separator.to_string())
                            )
                        }
                    }
                    Transform::Extreme { highest } => format!(
                        "sort {} -- {f} | head -n 1",
                        if *highest { "-nr" } else { "-n" }
                    ),
                    Transform::Sort {
                        numeric,
                        reverse,
                        unique,
                    } => {
                        let flags = format!(
                            "{}{}{}",
                            if *numeric { " -n" } else { "" },
                            if *reverse { " -r" } else { "" },
                            if *unique { " -u" } else { "" }
                        );
                        format!("sort{flags} -- {f}")
                    }
                    Transform::SortField {
                        index,
                        separator,
                        numeric,
                        reverse,
                    } => {
                        let key = format!(
                            "{index},{index}{}{}",
                            if *numeric { "n" } else { "" },
                            if *reverse { "r" } else { "" }
                        );
                        if *separator == '\t' {
                            format!("python3 -c 'import os,sys; os.execvp(\"sort\", [\"sort\", \"-t\", \"\\t\", \"-k\", *sys.argv[1:]])' {key} -- {f}")
                        } else {
                            format!(
                                "sort -t {} -k {key} -- {f}",
                                shell_quote(&separator.to_string())
                            )
                        }
                    }
                    Transform::Nonempty => format!("grep -v -e '^$' -- {f}"),
                    Transform::Match {
                        pattern,
                        regex,
                        exclude,
                        output,
                    } => match_command(&f, pattern, *regex, *exclude, output),
                };
                if let Some(dest) = destination {
                    use std::os::unix::fs::MetadataExt;
                    let name = &dest.path;
                    let d = cwd.join(name);
                    if let Ok(md) = d.symlink_metadata() {
                        if !dest.overwrite || !md.is_file() {
                            return Err(format!("Destination {name} already exists. Specify whether to overwrite it."));
                        }
                        if cwd
                            .join(input)
                            .metadata()
                            .is_ok_and(|src| (src.dev(), src.ino()) == (md.dev(), md.ino()))
                        {
                            return Err(format!("Destination {name} is also the input file."));
                        }
                    }
                    if !d.parent().is_some_and(Path::is_dir) {
                        return Err(format!("Destination directory does not exist: {name}"));
                    }
                    cmd.push_str(&format!(" > {}", path(name)));
                }
                Ok(cmd)
            }
            Self::Search {
                scope,
                shallow,
                predicate,
                exclude,
            } => {
                if !cwd.join(scope).is_dir() {
                    return Err(format!("Search directory does not exist: {scope}"));
                }
                if let Search::Content(p, regex) = predicate {
                    if *shallow || exclude.is_some() {
                        return Err("Which file selection should the content search use?".into());
                    }
                    return Ok(match_command(
                        &path(scope),
                        p,
                        *regex,
                        false,
                        &MatchOutput::Files,
                    ));
                }
                let mut cmd = format!("find {}", path(scope));
                if *shallow {
                    cmd.push_str(" -maxdepth 1");
                }
                cmd.push_str(&match predicate {
                    Search::Link => " -type l".into(),
                    Search::Empty => " -type f -empty".into(),
                    Search::Name(p) => format!(" -type f -name {}", shell_quote(p)),
                    Search::Larger(n) => format!(" -type f -size +{n}c"),
                    Search::Content(..) => unreachable!(),
                });
                if let Some(skip) = exclude {
                    cmd.push_str(&format!(" ! -name {}", shell_quote(skip)));
                }
                Ok(cmd)
            }
        }
    }
}

fn match_command(
    file: &str,
    pattern: &str,
    regex: bool,
    exclude: bool,
    output: &MatchOutput,
) -> String {
    let mode = if regex { "-E" } else { "-F" };
    let negative = if exclude { " -v" } else { "" };
    let flag = match output {
        MatchOutput::Files => " -rl",
        MatchOutput::Numbers => " -n",
        MatchOutput::Count => " -c",
        MatchOutput::Lines => "",
    };
    let suffix = if *output == MatchOutput::Numbers {
        " | cut -d : -f 1"
    } else {
        ""
    };
    format!(
        "grep {mode}{negative}{flag} -e {} -- {file}{suffix}",
        shell_quote(pattern)
    )
}

/// A vague, single-file quantity can use a compact model response. Only this
/// independently grounded unit and source can become an executable preview.
pub(crate) struct CountEvidence {
    pub unit: &'static str,
    pub path: String,
    flag: &'static str,
}

impl CountEvidence {
    pub fn from_request(request: &str, raw: &crate::context::RawScan) -> Option<Self> {
        let tokens = shlex::split(request.trim().trim_end_matches('?'))?;
        let mut path = None;
        let mut unit = None;
        let mut asks_quantity = false;
        for token in tokens {
            if raw
                .entries
                .iter()
                .any(|e| e.path == token && raw.environment.cwd.join(&e.path).is_file())
            {
                if path.replace(token).is_some() {
                    return None;
                }
                continue;
            }
            let word = token.to_lowercase();
            let value = match word.as_str() {
                "word" | "words" => Some(("words", "-w")),
                "line" | "lines" => Some(("lines", "-l")),
                "bytes" | "byte" => Some(("bytes", "-c")),
                "characters" | "character" => Some(("characters", "-m")),
                "count" | "total" | "tally" | "many" | "number" => {
                    asks_quantity = true;
                    None
                }
                "please" | "you" | "can" | "could" | "would" | "tell" | "me" | "give" | "get"
                | "what" | "what's" | "how" | "is" | "there" | "are" | "does" | "have" | "in"
                | "for" | "of" | "the" | "a" | "file" | "this" | "its" | "just" | "only" | "it"
                | "has" | "with" | "show" | "print" => None,
                _ => return None,
            };
            if let Some(v) = value {
                if unit.is_some_and(|previous| previous != v) {
                    return None;
                }
                unit = Some(v);
            }
        }
        if !asks_quantity {
            return None;
        }
        let (unit, flag) = unit?;
        Some(Self {
            unit,
            flag,
            path: path?,
        })
    }

    pub fn command(&self, cwd: &Path) -> Result<String, String> {
        Query::Stream {
            input: self.path.clone(),
            operation: Transform::Count(self.flag),
            destination: None,
        }
        .render(cwd)
    }
}
