fn main() -> anyhow::Result<()> {
    agentd::cli::main(
        agentd::attach::SelfLauncher::standalone()?,
        std::env::args().skip(1),
    );
    Ok(())
}
