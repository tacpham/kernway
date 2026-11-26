//! The driver's own binding to SQLite's C API keeps every `unsafe` in two
//! files: `ffi.rs` (the declarations) and `raw.rs` (the safe wrappers, each
//! block with its SAFETY reasoning). Elsewhere `unsafe` appears only where a
//! caller opts in to loading native extensions.

use std::path::Path;

const ALLOWED_FILES: &[&str] = &["ffi.rs", "raw.rs"];
/// The extension opt-in: the public unsafe constructor and its one call.
const ALLOWED_LINES: &[&str] = &["pub unsafe fn extension(", "unsafe { conn.load_extension("];

#[test]
fn unsafe_code_lives_only_in_the_binding() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if ALLOWED_FILES.contains(&name.as_str()) || path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        for (i, line) in std::fs::read_to_string(&path).unwrap().lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if code.contains("unsafe") && !ALLOWED_LINES.iter().any(|a| code.contains(a)) {
                offenders.push(format!("{name}:{}: {}", i + 1, line.trim()));
            }
        }
    }
    assert!(offenders.is_empty(), "unsafe outside the binding:\n{}", offenders.join("\n"));
    // And every unsafe block in raw.rs says why it is sound.
    let raw = std::fs::read_to_string(src.join("raw.rs")).unwrap();
    let lines: Vec<&str> = raw.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.contains("unsafe {") && !line.trim_start().starts_with("//") {
            let above = &lines[i.saturating_sub(4)..i];
            let says = above.iter().chain(std::iter::once(line)).any(|l| l.contains("SAFETY"));
            assert!(says, "raw.rs:{}: unsafe block without a SAFETY comment", i + 1);
        }
    }
}
