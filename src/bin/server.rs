use colored::Colorize;
use regex::Regex;
use std::io::Write;
use tokio::time::Instant;

pub fn log_init() {
    let launch_time = Instant::now();
    let regex = Regex::new("(.*star_racer.*)").unwrap();

    let mut binding = env_logger::builder();
    let builder = binding.format(move |buf, record| {
        let target_str = record.target();
        let mut results = vec![];
        for (_, [target]) in regex.captures_iter(target_str).map(|c| c.extract()) {
            results.push(target);
        }

        if results.len() != 1 {
            return write!(buf, "");
        }

        let now_time = Instant::now();
        let elapsed = now_time - launch_time;
        let elapsed = elapsed.as_millis() as f32 / 1000.;

        let args_str = format!("{}", record.args());

        writeln!(
            buf,
            "{:>8}|{}",
            elapsed.to_string().truecolor(255, 255, 255),
            // thread_id_str.truecolor(179, 7, 156),
            args_str,
        )
    });
    builder.init();
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    log_init();

    star_racer_server::run::run(8080).await?; // Use a default port of 8080

    anyhow::Ok(())
}
