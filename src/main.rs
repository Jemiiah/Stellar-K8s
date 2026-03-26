use std::process::{self, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::Utc;
use clap::{Parser, Subcommand};
use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use kube::api::{Api, ObjectMeta, Patch, PatchParams, PostParams};
use stellar_k8s::{controller, crd::StellarNode, preflight, Error};
use tracing::{debug, info, warn, Level};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Stellar-K8s: Cloud-Native Kubernetes Operator for Stellar Infrastructure",
    long_about = "stellar-operator manages StellarNode custom resources on Kubernetes.\n\n\
        EXAMPLES:\n  \
        stellar-operator run --namespace stellar-system\n  \
        stellar-operator logs --namespace stellar-system -f\n  \
        stellar-operator logs -f\n  \
        stellar-operator webhook --bind 0.0.0.0:8443\n  \
        stellar-operator info --namespace stellar-system\n  \
        stellar-operator version"
)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the operator reconciliation loop
    Run(RunArgs),
    /// Get logs from the stellar-operator pod(s)
    Logs(OperatorLogsArgs),
    /// Run the admission webhook server
    Webhook(WebhookArgs),
    /// Show version and build information
    Version,
    /// Show cluster information (node count) for a namespace
    Info(InfoArgs),
    /// Local simulator (kind/k3s + operator + demo validators)
    Simulator(SimulatorCli),
}

#[derive(Parser, Debug)]
struct OperatorLogsArgs {
    /// Kubernetes namespace where stellar-operator deployment runs
    ///
    /// Defaults to current context namespace
    #[arg(short, long, default_value = "default")]
    namespace: String,
    
    /// Container name within operator pod (default: 'operator')
    #[arg(short, long, default_value = "operator")]
    container: String,
    
    /// Follow log output as it comes in
    #[arg(short = 'f', long)]
    follow: bool,
    
    /// Number of lines to show from the end of logs
    #[arg(short, long, default_value_t = 100i64)]
    tail: i64,
    
    /// Pod name (if specific pod, default: all operator pods)
    #[arg(short, long)]
    pod: Option<String>,
}

#[derive(Parser, Debug)]
#[command(
    about = "Run the operator reconciliation loop",
    long_about = "Starts the main operator process that watches StellarNode resources and reconciles\n\
        their desired state. Supports leader election, optional mTLS for the REST API,\n\
        dry-run mode, and a latency-aware scheduler mode.\n\n\
        EXAMPLES:\n  \
        stellar-operator run\n  \
        stellar-operator run --namespace stellar-system\n  \
        stellar-operator run --namespace stellar-system --enable-mtls\n  \
        stellar-operator run --namespace stellar-system --dry-run\n  \
        stellar-operator run --namespace stellar-system --scheduler --scheduler-name my-scheduler\n  \
        stellar-operator run --dump-config\n\n\
        NOTE: --scheduler and --dry-run are mutually exclusive."
)]
struct RunArgs {
    /// Enable mutual TLS for the REST API.
    ///
    /// When set, the operator provisions a CA and server certificate in the target namespace,
    /// and the REST API requires client certificates signed by that CA.
    /// Env: ENABLE_MTLS
    #[arg(long, env = "ENABLE_MTLS")]
    enable_mtls: bool,

    /// Kubernetes namespace to watch and manage StellarNode resources in.
    ///
    /// Must match the namespace where StellarNode CRs are deployed.
    /// Env: OPERATOR_NAMESPACE
    ///
    /// Example: --namespace stellar-system
    #[arg(long, env = "OPERATOR_NAMESPACE", default_value = "default")]
    namespace: String,

    /// Restrict the operator to only watch and manage StellarNode resources in a specific namespace.
    ///
    /// When unset (default), the operator watches all namespaces and requires cluster-wide RBAC.
    /// When set, the operator only reconciles StellarNodes in this namespace and can run with
    /// namespace-scoped RBAC (Role/RoleBinding).
    /// Env: WATCH_NAMESPACE
    ///
    /// Example: --watch-namespace stellar-prod
    #[arg(long, env = "WATCH_NAMESPACE")]
    watch_namespace: Option<String>,

    /// Simulate reconciliation without applying any changes to the cluster.
    ///
    /// All reconciliation logic runs normally, but no Kubernetes API write calls are made.
    /// Useful for validating operator behaviour before a production rollout.
    /// Mutually exclusive with --scheduler.
    /// Env: DRY_RUN
    ///
    /// Example: --dry-run
    #[arg(long, env = "DRY_RUN")]
    dry_run: bool,

    /// Run the latency-aware scheduler instead of the standard operator reconciler.
    ///
    /// The scheduler assigns pending pods to nodes based on measured network latency
    /// between Stellar validators. Only one mode (scheduler or operator) runs per process.
    /// Mutually exclusive with --dry-run.
    /// Env: RUN_SCHEDULER
    ///
    /// Example: --scheduler --scheduler-name stellar-scheduler
    #[arg(long, env = "RUN_SCHEDULER")]
    scheduler: bool,

    /// Name registered with the Kubernetes scheduler framework when --scheduler is active.
    ///
    /// This name must match the `schedulerName` field in pod specs that should be
    /// handled by this scheduler instance.
    /// Env: SCHEDULER_NAME
    ///
    /// Example: --scheduler-name stellar-latency-scheduler
    #[arg(long, env = "SCHEDULER_NAME", default_value = "stellar-scheduler")]
    scheduler_name: String,

    /// Print the resolved runtime configuration and exit without starting the operator.
    ///
    /// Loads the operator config from the path in STELLAR_OPERATOR_CONFIG (or the default
    /// /etc/stellar-operator/config.yaml), merges it with all CLI flags and environment
    /// variables, prints the result as YAML, and exits with code 0.
    ///
    /// Example: --dump-config
    #[arg(long)]
    dump_config: bool,
    /// Run preflight checks and exit without starting the operator

    /// Run preflight checks and exit without starting the operator.
    /// Env: PREFLIGHT_ONLY
    #[arg(long, env = "PREFLIGHT_ONLY")]
    preflight_only: bool,
}

impl RunArgs {
    /// Validate mutually exclusive flags and other constraints.
    /// Returns an error string suitable for display if validation fails.
    fn validate(&self) -> Result<(), String> {
        if self.scheduler && self.dry_run {
            return Err(
                "--scheduler and --dry-run are mutually exclusive: the scheduler mode does not \
                 perform reconciliation writes, so dry-run has no effect and the combination is \
                 likely a misconfiguration."
                    .to_string(),
            );
        }
        Ok(())
    }
}

#[derive(Parser, Debug)]
struct InfoArgs {
    /// Kubernetes namespace to query for StellarNode resources.
    ///
    /// Env: OPERATOR_NAMESPACE
    ///
    /// Example: --namespace stellar-system
    #[arg(long, env = "OPERATOR_NAMESPACE", default_value = "default")]
    namespace: String,
}

#[derive(clap::Subcommand, Debug)]
enum SimulatorCmd {
    /// Create cluster, install operator manifests, print health hints
    Up(SimulatorUpArgs),
}

#[derive(Parser, Debug)]
struct SimulatorCli {
    #[command(subcommand)]
    command: SimulatorCmd,
}

#[derive(Parser, Debug)]
#[command(
    about = "Spin up a local simulator cluster with demo validators",
    long_about = "Creates a local kind or k3s cluster, applies the StellarNode CRD and operator\n\
        manifests, and deploys demo validator StellarNode resources for local development.\n\n\
        EXAMPLES:\n  \
        stellar-operator simulator up\n  \
        stellar-operator simulator up --cluster-name my-cluster --namespace stellar-dev\n  \
        stellar-operator simulator up --use-k3s"
)]
struct SimulatorUpArgs {
    /// Name of the kind cluster to create.
    ///
    /// Ignored when --use-k3s is set (k3s manages its own cluster name).
    ///
    /// Example: --cluster-name stellar-dev
    #[arg(long, default_value = "stellar-sim")]
    cluster_name: String,

    /// Kubernetes namespace for the operator and demo StellarNode resources.
    ///
    /// Example: --namespace stellar-dev
    #[arg(long, default_value = "stellar-system")]
    namespace: String,

    /// Use k3s instead of kind when both are available in PATH.
    ///
    /// k3s must already be running; the simulator will use the current kubeconfig context.
    ///
    /// Example: --use-k3s
    #[arg(long, default_value_t = false)]
    use_k3s: bool,
}

#[derive(Parser, Debug)]
#[command(
    about = "Run the admission webhook server",
    long_about = "Starts the HTTPS admission webhook server that validates and mutates StellarNode\n\
        resources on admission. Requires a valid TLS certificate and key for production use.\n\n\
        EXAMPLES:\n  \
        stellar-operator webhook --bind 0.0.0.0:8443 --cert-path /tls/tls.crt --key-path /tls/tls.key\n  \
        stellar-operator webhook --bind 127.0.0.1:8443 --log-level debug\n\n\
        NOTE: Running without --cert-path / --key-path is only suitable for local development."
)]
struct WebhookArgs {
    /// Address and port the webhook HTTPS server will listen on.
    ///
    /// Use 0.0.0.0 to listen on all interfaces, or a specific IP to restrict access.
    /// Env: WEBHOOK_BIND
    ///
    /// Example: --bind 0.0.0.0:8443
    #[arg(long, env = "WEBHOOK_BIND", default_value = "0.0.0.0:8443")]
    bind: String,

    /// Path to the PEM-encoded TLS certificate file served by the webhook.
    ///
    /// Must be signed by the CA configured in the ValidatingWebhookConfiguration.
    /// Env: WEBHOOK_CERT_PATH
    ///
    /// Example: --cert-path /etc/webhook/tls/tls.crt
    #[arg(long, env = "WEBHOOK_CERT_PATH")]
    cert_path: Option<String>,

    /// Path to the PEM-encoded TLS private key file for the webhook certificate.
    ///
    /// Must correspond to the certificate provided via --cert-path.
    /// Env: WEBHOOK_KEY_PATH
    ///
    /// Example: --key-path /etc/webhook/tls/tls.key
    #[arg(long, env = "WEBHOOK_KEY_PATH")]
    key_path: Option<String>,

    /// Minimum log level emitted by the webhook server.
    ///
    /// Accepted values: trace, debug, info, warn, error.
    /// Env: LOG_LEVEL
    ///
    /// Example: --log-level debug
    #[arg(long, env = "LOG_LEVEL", default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let args = Args::parse();

    match args.command {
        Commands::Version => {
            println!("Stellar-K8s Operator v{}", env!("CARGO_PKG_VERSION"));
            println!("Build Date: {}", env!("BUILD_DATE"));
            println!("Git SHA: {}", env!("GIT_SHA"));
            println!("Rust Version: {}", env!("RUST_VERSION"));
            return Ok(());
        }
        Commands::Logs(log_args) => {
            return operator_logs(log_args);
        }
        Commands::Info(info_args) => {
            return run_info(info_args).await;
        }
        Commands::Run(run_args) => {
            if let Err(e) = run_args.validate() {
                eprintln!("error: {e}");
                process::exit(2);
            }
            return run_operator(run_args).await;
        }
        Commands::Webhook(webhook_args) => {
            return run_webhook(webhook_args).await;
        }
        Commands::Simulator(cli) => {
            return run_simulator(cli).await;
        }
    }
}

/// Get logs from the stellar-operator Deployment pods
fn operator_logs(args: OperatorLogsArgs) -> Result<(), Error> {
    println!("📋 stellar-operator logs (namespace: {}, container: {})", args.namespace, args.container);
    
    let mut cmd = Command::new("kubectl");
    cmd.arg("logs");
    cmd.arg("-n").arg(&args.namespace);
    cmd.arg("deployment/stellar-operator");
    cmd.arg("-c").arg(&args.container);
    cmd.arg("--tail").arg(args.tail.to_string());
    
    if args.follow {
        cmd.arg("-f");
    }
    
    if let Some(pod_name) = &args.pod {
        cmd.arg(pod_name);
    }
    
    let status = cmd.status()
        .map_err(|e| Error::ConfigError(format!("kubectl logs failed to spawn: {e}")))?;
    
    if !status.success() {
        let code = status.code().unwrap_or(-1);
        return Err(Error::ConfigError(format!("kubectl logs exited with code {code}")));
    }
    
    Ok(())
}

// [Rest of the file unchanged - run_simulator, simulator_up, run_cmd, temp_operator_yaml, run_info, run_webhook, run_operator, leader election, etc.]
