//! Print aggregate tool statistics for one transcript: `survey <claude|pi|codex> <file>`.
//! Names, counts and timings only; no transcript text is read into the output.
use wb_observe::{AgentKind, Observer};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let kind = match args.get(1).map(String::as_str) {
        Some("claude") => AgentKind::Claude,
        Some("pi") => AgentKind::Pi,
        Some("codex") => AgentKind::Codex,
        _ => {
            eprintln!("usage: survey <claude|pi|codex> <file>");
            std::process::exit(2);
        }
    };
    let mut o = Observer::new(kind, &args[2]);
    let started = std::time::Instant::now();
    loop {
        let r = o.poll().expect("poll");
        if !r.more {
            break;
        }
    }
    let i = &o.index;
    println!("parsed in {:?}: events={} turns={} open_calls={} orphan_results={} oversized_lines={}", started.elapsed(), i.events, i.turns(), i.open_calls(), i.orphan_results, o.tailer().oversized_lines);
    let b = i.breakdown();
    println!("breakdown tool_s={} model_s={} human_s={}", b.tool_ms / 1000, b.model_ms / 1000, b.human_ms / 1000);
    let t = i.totals();
    println!("usage records={} input={:?} output={:?} cache_read={:?} reasoning={:?}", t.records, t.input, t.output, t.cache_read, t.reasoning);
    for (name, s) in i.tools().into_iter().take(8) {
        println!("{name:<28} calls={:<5} done={:<5} err={:<4} unk={:<5} total_s={:<7} median_ms={:?}", s.calls, s.completed, s.errors, s.unknown_outcome, s.total_ms / 1000, s.median_ms());
    }
}
