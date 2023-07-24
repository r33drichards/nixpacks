use anyhow::Result;
use clap::{arg, Parser, Subcommand, ValueEnum};
use nixpacks::{
    create_docker_image, generate_build_plan, get_plan_providers,
    nixpacks::{
        builder::docker::DockerBuilderOptions,
        nix::pkg::Pkg,
        plan::{
            generator::GeneratePlanOptions,
            phase::{Phase, StartPhase},
            BuildPlan,
        },
    },
};
use std::{
    collections::hash_map::DefaultHasher,
    env,
    hash::{Hash, Hasher},
    ops::Deref,
    string::ToString, io::Read, 
};

use ssh2::Session;
use std::fs::File;
use std::io::Write;
use std::net::TcpStream;
use std::path::Path;
use git2::Repository;
use std::error::Error;

/// The build plan config file format to use.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum PlanFormat {
    Json,
    Toml,
}

/// Arguments passed to `nixpacks`.
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
struct Args {
    #[command(subcommand)]
    command: Commands,

    /// Specify an entire build plan in json format that should be used to configure the build
    #[arg(long, global = true)]
    json_plan: Option<String>,

    /// Specify the install command to use
    #[arg(long, short, global = true)]
    install_cmd: Option<String>,

    /// Specify the build command to use
    #[arg(long, short, global = true)]
    build_cmd: Option<String>,

    /// Specify the start command to use
    #[arg(long, short, global = true)]
    start_cmd: Option<String>,

    /// Provide additional nix packages to install in the environment
    #[arg(long, short, global = true)]
    pkgs: Vec<String>,

    /// Provide additional apt packages to install in the environment
    #[arg(long, short, global = true)]
    apt: Vec<String>,

    /// Provide additional nix libraries to install in the environment
    #[arg(long, global = true)]
    libs: Vec<String>,

    /// Provide environment variables to your build
    #[arg(long, short, global = true)]
    env: Vec<String>,

    /// Path to config file
    #[arg(long, short, global = true)]
    config: Option<String>,
}

/// The valid subcommands passed to `nixpacks`, and their arguments.
#[derive(Subcommand)]
enum Commands {
    /// Generate a build plan for an app
    Plan {
        /// App source
        path: String,

        /// Specify the output format of the build plan.
        #[arg(short, long, value_enum, default_value = "json")]
        format: PlanFormat,
    },
    Devenv {
        /// App source
        path: String,
        hostname: String,
    },

    /// List all of the providers that will be used to build the app
    Detect {
        /// App source
        path: String,
    },

    /// Build an app
    Build {
        /// App source
        path: String,

        /// Name for the built image
        #[arg(short, long)]
        name: Option<String>,

        /// Save output directory instead of building it with Docker
        #[arg(short, long)]
        out: Option<String>,

        /// Print the generated Dockerfile to stdout
        #[arg(short, long, hide = true)]
        dockerfile: bool,

        /// Additional tags to add to the output image
        #[arg(short, long)]
        tag: Vec<String>,

        /// Additional labels to add to the output image
        #[arg(short, long)]
        label: Vec<String>,

        /// Set target platform for your output image
        #[arg(long)]
        platform: Vec<String>,

        /// Unique identifier to key cache by. Defaults to the current directory
        #[arg(long)]
        cache_key: Option<String>,

        /// Output Nixpacks related files to the current directory
        #[arg(long)]
        current_dir: bool,

        /// Disable building with the cache
        #[arg(long)]
        no_cache: bool,

        /// Image to hold the cached directories between builds.
        #[arg(long)]
        incremental_cache_image: Option<String>,

        /// Image to consider as cache sources
        #[arg(long)]
        cache_from: Option<String>,

        /// Enable writing cache metadata into the output image
        #[arg(long)]
        inline_cache: bool,

        /// Do not error when no start command can be found
        #[arg(long)]
        no_error_without_start: bool,

        /// Display more info during build
        #[arg(long, short)]
        verbose: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let pkgs = args
        .pkgs
        .iter()
        .map(|p| p.deref())
        .map(Pkg::new)
        .collect::<Vec<_>>();

    // CLI build plan
    let mut cli_plan = BuildPlan::default();
    if !args.pkgs.is_empty() || !args.libs.is_empty() || !args.apt.is_empty() {
        let mut setup = Phase::setup(Some(vec![pkgs, vec![Pkg::new("...")]].concat()));
        setup.apt_pkgs = Some(vec![args.apt, vec!["...".to_string()]].concat());
        setup.nix_libs = Some(vec![args.libs, vec!["...".to_string()]].concat());
        cli_plan.add_phase(setup);
    }
    if let Some(install_cmds) = args.install_cmd {
        let mut install = Phase::install(None);
        install.cmds = Some(vec![install_cmds]);
        cli_plan.add_phase(install);
    }
    if let Some(build_cmds) = args.build_cmd {
        let mut build = Phase::build(None);
        build.cmds = Some(vec![build_cmds]);
        cli_plan.add_phase(build);
    }
    if let Some(start_cmd) = args.start_cmd {
        let start = StartPhase::new(start_cmd);
        cli_plan.set_start_phase(start);
    }

    let json_plan = args.json_plan.map(BuildPlan::from_json).transpose()?;

    // Merge the CLI build plan with the json build plan
    let cli_plan = if let Some(json_plan) = json_plan {
        BuildPlan::merge_plans(&[json_plan, cli_plan])
    } else {
        cli_plan
    };

    let env: Vec<&str> = args.env.iter().map(|e| e.deref()).collect();
    let options = GeneratePlanOptions {
        plan: Some(cli_plan),
        config_file: args.config,
    };

    match args.command {
        // Produce a build plan for a project and print it to stdout.
        Commands::Plan { path, format } => {
            let plan = generate_build_plan(&path, env, &options)?;

            let plan_s = match format {
                PlanFormat::Json => plan.to_json()?,
                PlanFormat::Toml => plan.to_toml()?,
            };

            println!("{plan_s}");
        }

        Commands::Devenv { path, hostname } => {
            let plan = generate_build_plan(&path, env, &options)?;
            // let plan_s = plan.to_json()?;
            let packages = plan.get_packages();
            let home_manager_config = to_home_manager_nix(packages);
            // print home manager config
            print!("{home_manager_config}");
            // upload home_manager_config to remote host
            print!("uploading home manager config to remote host");

            let tcp = TcpStream::connect(hostname+":22").unwrap();
            let mut sess = Session::new().unwrap();
                // Use the TCP stream to start an SSH session
            sess.set_tcp_stream(tcp);
            sess.handshake().unwrap();

            // Authenticate using a private key
            let key_path = Path::new("/Users/robertwendt/.ssh/nixos");
            // let mut private_key = File::open(&key_path).unwrap();
            sess.userauth_pubkey_file("root", None, key_path, None).unwrap();
            assert!(sess.authenticated());
 

            let mut f = sess.scp_send(Path::new("/home/ubuntu/.config/home-manager/home.nix"), 0o644, home_manager_config.clone().as_bytes().len() as u64, None).unwrap();
            
            f.write_all(home_manager_config.clone().as_bytes()).unwrap();
            
            print!("uploaded home manager config to remote host");

            print!("run home manager switch on remote host");
            let mut channel = sess.channel_session().unwrap();
            channel.exec("nix-shell '<home-manager>' -A install").unwrap();
            let mut s = String::new();
            channel.read_to_string(&mut s).unwrap();
            print!("{}", s);
            channel.wait_close().unwrap();
            print!("home manager switch done");

            // copy key_path to remote host
            print!("uploading private key to remote host");
            // read private key into string
            let mut private_key = String::new();
            File::open(&key_path).unwrap().read_to_string(&mut private_key).unwrap();
            let mut f = sess.scp_send(Path::new("/home/ubuntu/.ssh/id_rsa"), 0o644, private_key.as_bytes().len() as u64, None).unwrap();
            f.write_all(private_key.as_bytes()).unwrap();
            print!("uploaded private key to remote host");

            //  if path is a git repo, upload it to remote host
            let path = Path::new(&path);
            if !is_git_repo(path.clone()) {
                print!("path is not a git repo");
                print!("uploading path to remote host");

                // create a tar gz of path
                // let mut tar_gz = tar::Builder::new(Vec::new());
                // tar_gz.append_dir_all(path.file_name(), &path).unwrap();
                return Ok(());
            }

            // get git remote url
            let git_remote_url = get_git_remote_url(&Path::new(&path)).unwrap();
            print!("git remote url: {}", git_remote_url);

            // clone git repo on remote host
            print!("cloning git repo on remote host");
            let mut channel = sess.channel_session().unwrap();
            channel.exec(format!("git clone {}", git_remote_url).as_str()).unwrap();
            let mut s = String::new();
            channel.read_to_string(&mut s).unwrap();
            print!("{}", s);
            channel.wait_close().unwrap();
            print!("cloned git repo on remote host");

        }


        // Detect which providers should be used to build a project and print them to stdout.
        Commands::Detect { path } => {
            let providers = get_plan_providers(&path, env, &options)?;
            println!("{}", providers.join(", "));
        }
        // Generate a Dockerfile and builds a container, using any specified build options.
        Commands::Build {
            path,
            name,
            out,
            dockerfile,
            tag,
            label,
            platform,
            cache_key,
            current_dir,
            no_cache,
            incremental_cache_image,
            cache_from,
            inline_cache,
            no_error_without_start,
            verbose,
        } => {
            let verbose = verbose || args.env.contains(&"NIXPACKS_VERBOSE=1".to_string());

            // Default to absolute `path` of the source that is being built as the cache-key if not disabled
            let cache_key = if !no_cache && cache_key.is_none() {
                get_default_cache_key(&path)?
            } else {
                cache_key
            };

            let build_options = &DockerBuilderOptions {
                name,
                tags: tag,
                labels: label,
                out_dir: out,
                quiet: false,
                cache_key,
                no_cache,
                platform,
                print_dockerfile: dockerfile,
                current_dir,
                inline_cache,
                cache_from,
                no_error_without_start,
                incremental_cache_image,
                verbose,
            };
            create_docker_image(&path, env, &options, build_options).await?;
        }
    }

    Ok(())
}

/// Creates a key for storing image layers in the Docker cache.
fn get_default_cache_key(path: &str) -> Result<Option<String>> {
    let current_dir = env::current_dir()?;
    let source = current_dir.join(path).canonicalize();
    if let Ok(source) = source {
        let source_str = source.to_string_lossy().to_string();
        let mut hasher = DefaultHasher::new();
        source_str.hash(&mut hasher);

        let encoded_source = base64::encode(hasher.finish().to_be_bytes())
            .replace(|c: char| !c.is_alphanumeric(), "");

        Ok(Some(encoded_source))
    } else {
        Ok(None)
    }
}


fn is_git_repo(path: &Path) -> bool {
    let git_path = path.join(".git");
    return git_path.exists();
}

fn get_git_remote_url(path: &Path) -> Result<String, Box<dyn Error>> {
    // Open the repository
    let repo = Repository::open(path)?;

    // Get the remote called "origin"
    let remote = repo.find_remote("origin")?;

    // Get the URL of the remote
    Ok(remote.url().unwrap_or_default().to_string())
}


fn to_home_manager_nix(packages: Vec<String>) -> String {
    // filter npm from packages 
    let packages = packages.into_iter().filter(|p| !p.contains("npm")).collect::<Vec<_>>();
    let mut text = "
    { config, pkgs, lib, ... }:

    {
      # Home Manager needs a bit of information about you and the paths it should
      # manage.
      home.username = \"ubuntu\";
      home.homeDirectory = \"/home/ubuntu\";
    
      # This value determines the Home Manager release that your configuration is
      # compatible with. This helps avoid breakage when a new Home Manager release
      # introduces backwards incompatible changes.
      #
      # You should not change this value, even if you update Home Manager. If you do
      # want to update the value, then make sure to first check the Home Manager
      # release notes.
      home.stateVersion = \"23.05\"; # Please read the comment before changing.
    
      # The home.packages option allows you to install Nix packages into your
      # environment.
      home.packages = with pkgs; [ 
".to_string();
    for package in &packages {
        // append pkgs.package to text 
        text = format!("{}        {} \n", text, package);
    }
    text.push_str("    ];\n}\n");

    return text;
}
