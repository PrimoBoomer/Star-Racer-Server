use colored::Colorize;
use std::io::Write;
use tokio::time::Instant;

pub fn log_init() {
    let launch_time = Instant::now();

    let mut binding = env_logger::builder();
    let builder = binding.format(move |buf, record| {
        let target_str = record.target();
        if !target_str.contains("star_racer") {
            return write!(buf, "");
        }

        let now_time = Instant::now();
        let elapsed = now_time - launch_time;
        let elapsed = elapsed.as_millis() as f32 / 1000.;

        let args_str = format!("{}", record.args());

        writeln!(buf, "{:>8}|{}", elapsed.to_string().truecolor(255, 255, 255), args_str,)
    });
    builder.init();
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    log_init();

    star_racer_server::run::run(8080).await?;

    anyhow::Ok(())
}
