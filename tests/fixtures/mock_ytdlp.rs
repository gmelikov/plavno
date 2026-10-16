use std::{
    fs::{self, OpenOptions},
    io::Write,
    process,
};

fn main() {
    let executable = std::env::current_exe().unwrap();
    let directory = executable.parent().unwrap();
    let calls = directory.join("calls");
    let number = fs::read_to_string(&calls)
        .unwrap_or_default()
        .lines()
        .count()
        + 1;
    writeln!(
        OpenOptions::new()
            .append(true)
            .create(true)
            .open(calls)
            .unwrap(),
        "{}",
        std::env::args().skip(1).collect::<Vec<_>>().join("\0")
    )
    .unwrap();
    let read =
        |suffix: &str| fs::read(directory.join(format!("{number}.{suffix}"))).unwrap_or_default();
    std::io::stdout().write_all(&read("json")).unwrap();
    std::io::stderr().write_all(&read("err")).unwrap();
    let status = fs::read_to_string(directory.join(format!("{number}.status")))
        .unwrap_or_default()
        .trim()
        .parse()
        .unwrap_or(0);
    process::exit(status);
}
