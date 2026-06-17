use std::path::PathBuf;
use std::{net::SocketAddr, sync::Arc};

use astra_runner::{
    LocalRunnerConfig, LocalRunnerEnvironment, local_runner_advertisement,
    local_runner_register_request, runner_rpc_router,
};
use astra_runtime_env::{RuntimeEnvironment, RuntimeSessionSpec, RuntimeToolInvocation};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::Value;

#[derive(Parser, Debug)]
#[command(name = "astra-runner", version, about)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print this runner's runtime environment advertisement as JSON.
    Advertise(LocalRunnerArgs),
    /// Print this runner's registration payload as JSON.
    RegisterPayload(LocalRunnerArgs),
    /// Execute one tool locally through the runner contract.
    ExecuteOnce(ExecuteOnceArgs),
    /// Serve the runner RPC API.
    Serve(ServeArgs),
}

#[derive(clap::Args, Debug, Clone)]
struct LocalRunnerArgs {
    /// Runner identifier advertised to the control plane.
    #[arg(long, env = "ASTRA_RUNNER_ID", default_value = "local-runner")]
    runner_id: String,

    /// Owner user id for personal runner registration.
    #[arg(long, env = "ASTRA_RUNNER_OWNER_ID")]
    owner_id: Option<String>,

    /// Workspace mounted into this runner.
    #[arg(long, env = "ASTRA_WORKSPACE_DIR", default_value = ".")]
    workspace_dir: PathBuf,

    /// Workspace authority granted to this runner.
    #[arg(long, value_enum, default_value_t = WorkspaceAuthorityArg::ReadWrite)]
    authority: WorkspaceAuthorityArg,

    /// Base URL the control plane should use to call this runner.
    #[arg(long, env = "ASTRA_RUNNER_RPC_BASE_URL")]
    rpc_base_url: Option<String>,
}

#[derive(clap::Args, Debug, Clone)]
struct ExecuteOnceArgs {
    #[command(flatten)]
    runner: LocalRunnerArgs,

    /// Tool name to execute.
    #[arg(long)]
    tool: String,

    /// JSON tool arguments.
    #[arg(long, default_value = "{}")]
    args: String,

    /// Tool call id for result evidence.
    #[arg(long, default_value = "call-1")]
    call_id: String,

    /// Runtime session id.
    #[arg(long, default_value = "session-1")]
    session_id: String,

    /// Run id.
    #[arg(long, default_value = "run-1")]
    run_id: String,
}

#[derive(clap::Args, Debug, Clone)]
struct ServeArgs {
    #[command(flatten)]
    runner: LocalRunnerArgs,

    /// Address for the runner RPC service.
    #[arg(long, env = "ASTRA_RUNNER_BIND", default_value = "127.0.0.1:3847")]
    bind: SocketAddr,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum WorkspaceAuthorityArg {
    ReadOnly,
    ReadWrite,
}

impl From<WorkspaceAuthorityArg> for astra_runtime_env::WorkspaceAuthority {
    fn from(value: WorkspaceAuthorityArg) -> Self {
        match value {
            WorkspaceAuthorityArg::ReadOnly => Self::ReadOnly,
            WorkspaceAuthorityArg::ReadWrite => Self::ReadWrite,
        }
    }
}

impl From<LocalRunnerArgs> for LocalRunnerConfig {
    fn from(value: LocalRunnerArgs) -> Self {
        let mut config = LocalRunnerConfig::new(value.runner_id, value.workspace_dir)
            .with_authority(value.authority.into());
        if let Some(owner_id) = value.owner_id {
            config = config.with_owner_id(owner_id);
        }
        if let Some(rpc_base_url) = value.rpc_base_url {
            config = config.with_rpc_base_url(rpc_base_url);
        }
        config
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    match args.command {
        Command::Advertise(args) => {
            let advert = local_runner_advertisement(&args.into());
            println!("{}", serde_json::to_string_pretty(&advert)?);
        }
        Command::RegisterPayload(args) => {
            let request = local_runner_register_request(&args.into());
            println!("{}", serde_json::to_string_pretty(&request)?);
        }
        Command::ExecuteOnce(args) => {
            let config = LocalRunnerConfig::from(args.runner);
            let env = LocalRunnerEnvironment::new(config);
            let binding = env.binding();
            let session = RuntimeEnvironment::prepare_session(
                &env,
                RuntimeSessionSpec::new(args.session_id, args.run_id, binding.clone())
                    .with_requested_tools([args.tool.clone()]),
            )
            .await?;
            let tool_args: Value = serde_json::from_str(&args.args)?;
            let outcome = RuntimeEnvironment::execute_tool(
                &env,
                &session,
                RuntimeToolInvocation::new(
                    args.call_id,
                    args.tool,
                    tool_args,
                    binding,
                    session.policy.revision,
                ),
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
        }
        Command::Serve(args) => {
            let listener = tokio::net::TcpListener::bind(args.bind).await?;
            let local_addr = listener.local_addr()?;
            let mut config = LocalRunnerConfig::from(args.runner);
            if config.rpc_base_url.is_none() {
                config = config.with_rpc_base_url(format!("http://{local_addr}"));
            }
            let env = Arc::new(LocalRunnerEnvironment::new(config));
            eprintln!("astra-runner RPC listening on {local_addr}");
            axum::serve(listener, runner_rpc_router(env)).await?;
        }
    }
    Ok(())
}
