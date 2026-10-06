//! Per-turn source coverage. File, line and symbol reads share the same
//! content-version key; failed reads and omitted output never become coverage.

use std::collections::BTreeMap;
use serde_json::{Value, json};

#[derive(Default)]
pub struct ReadCoverage {
    files: BTreeMap<String, FileCoverage>,
}

struct FileCoverage {
    hash: String,
    ranges: Vec<(u64, u64)>,
}

pub fn is_source_read(name: &str) -> bool {
    matches!(name, "read_file" | "read_file_lines" | "read_ts_symbol"
        | "read_rust_symbol" | "read_go_symbol" | "read_python_symbol")
}

fn normalized_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    #[cfg(windows)]
    { normalized.to_lowercase() }
    #[cfg(not(windows))]
    { normalized }
}

fn ranges(lines: &[u64]) -> Vec<(u64, u64)> {
    let mut out = Vec::<(u64, u64)>::new();
    for &line in lines {
        if let Some(last) = out.last_mut() {
            if line <= last.1.saturating_add(1) {
                last.1 = last.1.max(line);
                continue;
            }
        }
        out.push((line, line));
    }
    out
}

impl ReadCoverage {
    pub fn clear(&mut self) { self.files.clear(); }

    pub fn forget(&mut self,path:&str) {self.files.remove(&normalized_path(path));}

    /// Coverage describes returned tool pages, not conclusions learned by the
    /// model. Preserve the tool body: deduplication happens when constructing
    /// the actual request against its final selected source pages.
    pub fn filter(&mut self,name:&str,args:&Value,result:&mut Value)->bool {
        if !is_source_read(name) {return false;}
        let Some(path)=result.get("path").or_else(||result.pointer("/symbol/file_path")).and_then(Value::as_str).map(normalized_path) else {return false;};
        let Some(hash)=result.get("code_hash").or_else(||result.pointer("/description/code_hash")).and_then(Value::as_str).map(str::to_owned) else {return false;};
        let Some(content)=result["content"].as_str() else {return false;};
        let count=content.lines().count();
        if count==0 || result["partial_line"]==true {return false;}
        let start=result.get("start_line").or_else(||result.pointer("/symbol/start_line")).and_then(Value::as_u64).unwrap_or(1);
        let end=start+count as u64-1;
        if self.files.len()>=128 && !self.files.contains_key(&path) {self.files.clear();}
        let changed=self.files.get(&path).is_some_and(|entry|entry.hash!=hash);
        let entry=self.files.entry(path.clone()).or_insert_with(||FileCoverage{hash:hash.clone(),ranges:Vec::new()});
        if changed {entry.hash=hash.clone();entry.ranges.clear();}
        let old=(start..=end).filter(|line|entry.ranges.iter().any(|&(a,b)|a<=*line && *line<=b)).collect::<Vec<_>>();
        let repeated=!old.is_empty() && args["force_read"]!=true;
        let material_end=result["material_end_line"].as_u64().unwrap_or(end);
        result["read_coverage"]=json!({"status":if args["force_read"]==true {"forced"} else if changed {"refreshed"} else if repeated {"repeated"} else {"new"},
            "path":path,"code_hash":hash,"returned_ranges":vec![(start,end)],
            "previously_read_ranges":ranges(&old),"not_returned_ranges":if material_end>end {vec![(end+1,material_end)]}else{Vec::new()},
            "omitted_lines":0,"truncated":result["complete"]==false,
            "guidance":"Coverage tracks returned pages, not understanding. Tool source remains intact; duplicate bodies are projected once when constructing the actual request against the final selected pages. Use already supplied code for the next action rather than repeat discovery."});
        let mut spans=entry.ranges.clone();spans.push((start,end));spans.sort_unstable();
        let mut merged=Vec::<(u64,u64)>::new();
        for (a,b) in spans {if let Some(last)=merged.last_mut().filter(|last|a<=last.1.saturating_add(1)){last.1=last.1.max(b);}else{merged.push((a,b));}}
        entry.ranges=merged;
        repeated
    }

    #[cfg(test)]
    pub fn summary(&self) -> String {
        self.files.iter().take(20).map(|(path, entry)| {
            let spans = entry.ranges.iter().take(12).map(|(start, end)| format!("{start}-{end}"))
                .collect::<Vec<_>>().join(", ");
            format!("{path}: lines {spans}; code_hash={}", entry.hash)
        }).collect::<Vec<_>>().join("\n")
    }
}
