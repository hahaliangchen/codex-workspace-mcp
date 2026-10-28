//! Pure path and URL operations for the flow runtime; no process or filesystem access.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};

const INPUT_LIMIT: usize = 64 * 1024;
const OUTPUT_LIMIT: usize = 256 * 1024;

pub(super) fn arity(name: &str) -> Option<(usize, usize)> {
    Some(match name {
        "path.join" | "path.resolve" => (1, 32),
        "path.relative" => (2, 2),
        "path.basename" | "url.parse" => (1, 2),
        "path.normalize" | "path.dirname" | "path.extname" => (1, 1),
        _ => return None,
    })
}

fn separator(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

// Lexical normalization only: does not touch the filesystem or follow symlinks.
fn normalize(input: &str, preserve_trailing: bool) -> String {
    let mut prefix = PathBuf::new();
    let mut parts: Vec<String> = Vec::new();
    let mut rooted = false;
    for component in Path::new(input).components() {
        match component {
            Component::Prefix(p) => prefix.push(p.as_os_str()),
            Component::RootDir => {
                prefix.push(std::path::MAIN_SEPARATOR.to_string());
                rooted = true;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.last().is_some_and(|p| p != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..".into());
                }
            }
            Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
        }
    }
    for part in parts {
        prefix.push(part);
    }
    let mut result = prefix.to_string_lossy().into_owned();
    if result.is_empty() {
        result.push('.');
    }
    if preserve_trailing
        && !input.is_empty()
        && input.chars().last().is_some_and(separator)
        && !result.chars().last().is_some_and(separator)
    {
        result.push(std::path::MAIN_SEPARATOR);
    }
    result
}

fn basename(input: &str) -> &str {
    let trimmed = input.trim_end_matches(separator);
    match trimmed.rfind(separator) {
        Some(index) => &trimmed[index + 1..],
        None => trimmed,
    }
}

#[cfg(windows)]
fn dirname(input: &str) -> String {
    if input.is_empty() {
        return ".".into();
    }
    let path = Path::new(input);
    if path.parent().is_none() && path.has_root() {
        return normalize(input, false);
    }
    let parent = path.parent().unwrap_or(Path::new(""));
    let text = parent.to_string_lossy();
    if text.is_empty() {
        ".".into()
    } else {
        text.into_owned()
    }
}

#[cfg(not(windows))]
fn dirname(input: &str) -> String {
    if input.is_empty() {
        return ".".into();
    }
    let root = input.starts_with(separator);
    let mut seen_non_separator = false;
    let mut end = None;
    for (index, ch) in input.char_indices().rev() {
        if index == 0 {
            break;
        }
        if separator(ch) {
            if seen_non_separator {
                end = Some(index);
                break;
            }
        } else {
            seen_non_separator = true;
        }
    }
    match end {
        None => {
            if root {
                std::path::MAIN_SEPARATOR.to_string()
            } else {
                ".".into()
            }
        }
        Some(1) if root && !cfg!(windows) => "//".into(),
        Some(index) => input[..index].into(),
    }
}

fn extension(input: &str) -> &str {
    let base = basename(input);
    if base == "." || base == ".." {
        return "";
    }
    match base.rfind('.') {
        Some(index) if index > 0 => &base[index..],
        _ => "",
    }
}

fn url_parse(input: &str, base: Option<&str>) -> Result<Value> {
    let parsed = match base {
        Some(base) => url::Url::parse(base).and_then(|base| base.join(input)),
        None => url::Url::parse(input),
    }
    .map_err(|error| anyhow::anyhow!("URL parse error: {error}"))?;
    let search = parsed
        .query()
        .filter(|value| !value.is_empty())
        .map(|value| format!("?{value}"))
        .unwrap_or_default();
    let hash = parsed
        .fragment()
        .filter(|value| !value.is_empty())
        .map(|value| format!("#{value}"))
        .unwrap_or_default();
    let hostname = parsed.host_str().unwrap_or("");
    let hostname = if hostname.contains(':') && !hostname.starts_with('[') {
        format!("[{hostname}]")
    } else {
        hostname.to_owned()
    };
    Ok(
        json!({"href": parsed.as_str(), "origin": parsed.origin().ascii_serialization(),
        "protocol": format!("{}:", parsed.scheme()), "hostname": hostname,
        "port": parsed.port().map(|v|v.to_string()).unwrap_or_default(), "pathname": parsed.path(),
        "search": search, "hash": hash}),
    )
}

pub(super) fn call(name: &str, args: Vec<Value>) -> Result<Value> {
    let (min, max) = arity(name).context("unsupported built-in")?;
    ensure!(
        (min..=max).contains(&args.len()),
        "invalid built-in argument count"
    );
    let strings = args
        .iter()
        .map(|v| v.as_str().context("built-in arguments must be strings"))
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        serde_json::to_vec(&json!({"name":name,"args":args}))?.len() <= INPUT_LIMIT,
        "built-in input exceeds 64 KiB"
    );
    let value = match name {
        "path.normalize" => json!(normalize(strings[0], true)),
        "path.join" => {
            let joined = strings
                .iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(&std::path::MAIN_SEPARATOR.to_string());
            json!(normalize(&joined, true))
        }
        "path.resolve" => {
            let index = strings
                .iter()
                .rposition(|part| Path::new(part).is_absolute())
                .context("path.resolve requires an absolute path argument")?;
            let joined = strings[index..].join(&std::path::MAIN_SEPARATOR.to_string());
            json!(normalize(&joined, false))
        }
        "path.relative" => {
            ensure!(
                strings.iter().all(|part| Path::new(part).is_absolute()),
                "path.relative requires absolute paths"
            );
            let a = normalize(strings[0], false);
            let b = normalize(strings[1], false);
            let left: Vec<_> = Path::new(&a).components().collect();
            let right: Vec<_> = Path::new(&b).components().collect();
            let mut common = 0;
            while common < left.len()
                && common < right.len()
                && if cfg!(windows) {
                    left[common]
                        .as_os_str()
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&right[common].as_os_str().to_string_lossy())
                } else {
                    left[common] == right[common]
                }
            {
                common += 1;
            }
            if common == 0 {
                json!(b)
            } else {
                let mut result = PathBuf::new();
                for _ in common..left.len() {
                    result.push("..");
                }
                for part in &right[common..] {
                    result.push(part.as_os_str());
                }
                json!(result.to_string_lossy().to_string())
            }
        }
        "path.dirname" => json!(dirname(strings[0])),
        "path.basename" => {
            let base = basename(strings[0]);
            json!(if strings.len() == 2 && !strings[1].is_empty() {
                base.strip_suffix(strings[1]).unwrap_or(base)
            } else {
                base
            })
        }
        "path.extname" => json!(extension(strings[0])),
        "url.parse" => url_parse(strings[0], strings.get(1).copied())?,
        _ => bail!("unsupported built-in"),
    };
    ensure!(
        serde_json::to_vec(&value)?.len() <= OUTPUT_LIMIT,
        "built-in output exceeds 256 KiB"
    );
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_arguments_and_large_inputs_without_external_runtime() {
        assert!(
            call("path.resolve", vec![json!("relative")])
                .unwrap_err()
                .to_string()
                .contains("absolute")
        );
        assert!(call("path.join", vec![json!(17)]).is_err());
        assert!(call("path.join", vec![json!("x".repeat(INPUT_LIMIT))]).is_err());
        assert!(call("url.parse", vec![json!("relative")]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn common_operations_match_node_when_available() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut cases = Vec::new();
        for value in [
            "",
            ".",
            "..",
            ".foo",
            "..foo",
            "...",
            "foo.",
            "foo..",
            "/",
            "//",
            "//a",
            "/a//b/",
            "a/../",
            "a/./b",
            "中文/../文件",
            "a/b/../c",
        ] {
            for name in [
                "path.normalize",
                "path.dirname",
                "path.basename",
                "path.extname",
            ] {
                cases.push(json!({"name":name,"args":[value]}));
            }
        }
        for (name, args) in [
            ("path.join", json!(["a", "/b", "..", "c/"])),
            ("path.join", json!(["", ""])),
            ("path.resolve", json!(["/project", "a", "../b/"])),
            ("path.relative", json!(["/a/b", "/a/c"])),
            ("path.relative", json!(["/a", "/a"])),
            ("path.basename", json!(["/a/file.txt", ".txt"])),
            (
                "url.parse",
                json!(["../a%20b.css?x=1#top", "https://example.com/docs/pages/"]),
            ),
            ("url.parse", json!(["https://EXAMPLE.com:443/a?x=1#z"])),
            ("url.parse", json!(["file:///tmp/a"])),
            ("url.parse", json!(["https://[::1]:8080/a?x=%20#part"])),
            ("url.parse", json!(["https://example.com/中文?x=1#文"])),
            ("url.parse", json!(["https://example.com/a?"])),
            ("url.parse", json!(["https://example.com/a#"])),
            ("url.parse", json!(["mailto:user@example.com"])),
        ] {
            cases.push(json!({"name":name,"args":args}));
        }
        let source = r#"const p=require('node:path');let b='';process.stdin.on('data',c=>b+=c);process.stdin.on('end',()=>{const v=JSON.parse(b);console.log(JSON.stringify(v.map(x=>{if(x.name==='url.parse'){const u=x.args.length===2?new URL(x.args[0],x.args[1]):new URL(x.args[0]);return {href:u.href,origin:u.origin,protocol:u.protocol,hostname:u.hostname,port:u.port,pathname:u.pathname,search:u.search,hash:u.hash}}return p[x.name.slice(5)](...x.args)})))})"#;
        let Ok(mut child) = Command::new("node")
            .arg("-e")
            .arg(source)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
        else {
            return;
        };
        child
            .stdin
            .take()
            .unwrap()
            .write_all(serde_json::to_string(&cases).unwrap().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let expected: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
        for (case, expected) in cases.iter().zip(expected) {
            let actual = call(
                case["name"].as_str().unwrap(),
                case["args"].as_array().unwrap().clone(),
            )
            .unwrap();
            assert_eq!(actual, expected, "case {case}");
        }
    }
}
