fn main() {
    use pir::wininfo::impls::*;
    let titles = window_titles();
    println!("window_titles count = {}", titles.len());
    for t in &titles { println!("  pid={} title='{}'", t.pid, t.title); }
    let clip = clipboard_text();
    println!("clipboard_text = {}", if clip.is_empty() { "<empty>".to_string() } else { format!("{} chars", clip.len()) });
}
