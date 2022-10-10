use bytes::Bytes;
use log::{error, trace};
use std::{
    collections::HashMap, fmt, fs, io, path, path::Path, process as std_process, thread, time,
};
use thiserror::Error;
use tokio::{process, select, sync::oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeromq::{Socket, SocketRecv, SocketSend, ZmqError};

#[derive(Error, Debug)]
pub enum OMCPathError {
    #[error("The path {path:?} could not be converted to string.")]
    PathToStringConversionFailed { path: path::PathBuf },
    #[error("The OpenModelica Compiler (OMC) was not found at '{omc_path}'.")]
    OMCNotFound { omc_path: String },
}

#[derive(Error, Debug)]
pub enum OMCSessionBaseConstructionError {
    #[error(transparent)]
    OMCPathErr(#[from] OMCPathError),
    #[error("Failed to create OMC log file at '{path}'. Source: {source:?}")]
    FailedToCreateOMCLogFile {
        path: String,
        source: std::io::Error,
    },
    #[error(transparent)]
    IOError(#[from] std::io::Error),
}

#[derive(Error, Debug)]
pub enum OMCSessionZMQConstructionError {
    #[error("The environment variable '{env_var}' is not set.")]
    OpenModelicaHomeEnvVarNotSet { env_var: String },
    #[error("OMC server failed to start. Could not open file '{port_file}'.")]
    OMCServerFailedToStart { port_file: String },
    #[error(transparent)]
    OMCSessionBaseConstructionErr(#[from] OMCSessionBaseConstructionError),
    #[error(transparent)]
    IOErr(#[from] std::io::Error),
    #[error(transparent)]
    OMCSessionZMQErr(#[from] OMCSessionZMQError),
}

#[derive(Error, Debug)]
pub enum OMCSessionZMQError {
    #[error("OMC expression error: {bytes:?}.")]
    ExpressionError { bytes: Vec<Bytes> },
    #[error(transparent)]
    FromZmqErr(#[from] FromZmqError),
    #[error(transparent)]
    ZmqErr(#[from] zeromq::ZmqError),
}

#[derive(Error, Debug)]
pub enum FromZmqError {
    #[error("Failed to parse model build results: {bytes:?}.")]
    BuildModelResultsParseError { bytes: Vec<Bytes> },
    #[error(transparent)]
    FromUtf8Err(#[from] std::string::FromUtf8Error),
}

#[derive(Error, Debug)]
pub enum ModelicaSystemError {
    #[error(transparent)]
    OMCPathErr(#[from] OMCPathError),
    #[error("The model at path '{model_path}' does not exist.")]
    ModelNotFound { model_path: String },
    #[error("The simulation executable at path '{exec_path}' does not exist.")]
    SimNotFound { exec_path: String },
    #[error("Build executable must be in some directory '{exec_path}'.")]
    ExecParentDirError { exec_path: String },
    #[error("Failed to create simulation log file at '{path}'. Source: {source:?}")]
    FailedToCreateSimLogFile {
        path: String,
        source: std::io::Error,
    },
    #[error(transparent)]
    OMCSessionZMQErr(#[from] OMCSessionZMQError),
    #[error(transparent)]
    IOError(#[from] std::io::Error),
}

/// Enum of supported OMC expressions.
#[derive(Debug, Clone)]
pub enum OMCExpression {
    Quit,
    GetErrorString,
    LoadFile { file: String },
    BuildModel { name: String },
}

impl fmt::Display for OMCExpression {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let expr_str: String = match self {
            Self::Quit => "quit()".to_string(),
            Self::GetErrorString => "getErrorString()".to_string(),
            Self::LoadFile { file } => format!("loadFile(\"{file}\")"),
            Self::BuildModel { name } => format!("buildModel({name})"),
        };
        write!(f, "{}", expr_str)
    }
}

pub struct OMCSessionBase {
    log_file_path: String,
    // Child process running the OMC interactive server. Killed on drop.
    _omc_process: process::Child,
}

impl OMCSessionBase {
    async fn new(
        omc_home: String,
        random_string: String,
        current_user: String,
        log_file_suffix: String,
        omc_args: Vec<&str>,
    ) -> Result<Self, OMCSessionBaseConstructionError> {
        trace!("Creating new OMC base session.");
        let omc_path = OMCSessionBase::get_omc_path_str(&omc_home)?;
        let log_file_path =
            format!("/tmp/openmodelica.{current_user}.{log_file_suffix}.{random_string}.log");
        let omc_log_file = fs::File::create(&log_file_path).map_err(|e| {
            OMCSessionBaseConstructionError::FailedToCreateOMCLogFile {
                path: log_file_path.clone(),
                source: e,
            }
        })?;
        let stdio = std_process::Stdio::from(omc_log_file);
        let omc_process = Self::start_omc_process(&omc_path, &omc_args, stdio).await?;
        let omc_session = OMCSessionBase {
            log_file_path,
            _omc_process: omc_process,
        };
        Ok(omc_session)
    }

    pub fn log_file_path(&self) -> &str {
        &self.log_file_path
    }

    async fn start_omc_process(
        omc_path: &str,
        omc_args: &Vec<&str>,
        stdio: std_process::Stdio,
    ) -> Result<process::Child, OMCSessionBaseConstructionError> {
        process::Command::new(omc_path)
            .args(omc_args)
            .stdout(stdio)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    OMCSessionBaseConstructionError::OMCPathErr(OMCPathError::OMCNotFound {
                        omc_path: omc_path.to_string(),
                    })
                }
                _ => e.into(),
            })
    }

    fn get_omc_path(omc_home: &str) -> Result<path::PathBuf, OMCPathError> {
        let path: path::PathBuf = [omc_home, "bin", "omc"].iter().collect();
        Ok(path)
    }

    fn get_omc_path_str(omc_home: &str) -> Result<String, OMCPathError> {
        OMCSessionBase::get_omc_path(omc_home).and_then(|p| {
            p.to_str()
                .ok_or_else(|| OMCPathError::PathToStringConversionFailed { path: p.clone() })
                .map(|s| s.to_string())
        })
    }
}

pub struct OMCSessionZMQ {
    port_file: String,
    base: OMCSessionBase,
    socket: zeromq::ReqSocket,
    current_user: String,
    log_file_suffix: String,
    random_string: String,
}

pub struct BuildModelResults {
    exec_path: String,
    xml_file_name: String,
}

pub trait FromZmq: Sized {
    /// Parses a ZMQ message to return a value of this type.
    fn from_zmq(input: zeromq::ZmqMessage) -> Result<Self, FromZmqError>;
}

impl FromZmq for BuildModelResults {
    fn from_zmq(input: zeromq::ZmqMessage) -> Result<Self, FromZmqError> {
        let opt_msg_str = input
            .get(0)
            .and_then(|b| String::from_utf8(b.to_vec()).ok());
        if let Some(msg_str) = opt_msg_str {
            let trimmed: &str = &msg_str.trim().replace('\"', "");
            let first_last_off: &str = &trimmed[1..trimmed.len() - 1];
            let omc_array: Vec<&str> = first_last_off.split(',').collect();
            if let (Some(exec_path), Some(xml_file_name)) = (omc_array.first(), omc_array.get(1)) {
                return Ok(Self {
                    exec_path: exec_path.to_string(),
                    xml_file_name: xml_file_name.to_string(),
                });
            }
        }
        Err(FromZmqError::BuildModelResultsParseError {
            bytes: input.into_vec(),
        })
    }
}

impl OMCSessionZMQ {
    pub async fn new(omc_home: Option<String>) -> Result<Self, OMCSessionZMQConstructionError> {
        trace!("Creating new OMC ZMQ session.");
        let home = omc_home.unwrap_or({
            let env_var = "OPENMODELICAHOME";
            std::env::var(env_var).map_err(|_| {
                OMCSessionZMQConstructionError::OpenModelicaHomeEnvVarNotSet {
                    env_var: env_var.to_string(),
                }
            })?
        });
        let current_user = whoami::username();
        let random_string = Uuid::new_v4().to_string();
        let log_file_suffix = "port".to_string();
        let port_file =
            format!("/tmp/openmodelica.{current_user}.{log_file_suffix}.{random_string}");
        let zeromq_file_suffix = format!("-z={random_string}");
        let omc_args = vec![
            "--interactive=zmq",
            "--locale=C",
            zeromq_file_suffix.as_str(),
        ];
        // Setup base session and spawn OMC process
        let session_base = OMCSessionBase::new(
            home,
            random_string.clone(),
            current_user.clone(),
            log_file_suffix.clone(),
            omc_args,
        )
        .await?;

        // Connect to OMC server
        let socket = Self::connect(&port_file, time::Duration::from_millis(5), 80).await?;

        let omc_zmq_session = OMCSessionZMQ {
            port_file,
            base: session_base,
            socket,
            current_user,
            log_file_suffix,
            random_string,
        };
        Ok(omc_zmq_session)
    }

    pub fn port_file(&self) -> &str {
        &self.port_file
    }

    pub fn base(&self) -> &OMCSessionBase {
        &self.base
    }

    async fn connect(
        port_file: &str,
        timeout: time::Duration,
        attempts: u8,
    ) -> Result<zeromq::ReqSocket, OMCSessionZMQConstructionError> {
        for _ in 0..attempts {
            match fs::read_to_string(port_file) {
                Ok(address) => {
                    trace!("Attempting to connect to address '{address}'.");
                    // TODO: Set socket options
                    let mut socket = zeromq::ReqSocket::new();
                    socket
                        .connect(&address)
                        .await
                        .map_err(|e| OMCSessionZMQConstructionError::OMCSessionZMQErr(e.into()))?;
                    trace!("Connected to OMC server at '{address}'.");
                    return Ok(socket);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    thread::sleep(timeout);
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(OMCSessionZMQConstructionError::OMCServerFailedToStart {
            port_file: port_file.to_string(),
        })
    }

    async fn quit(&mut self) -> Result<(), OMCSessionZMQError> {
        let _ = self.send_expression(OMCExpression::Quit).await?;
        Ok(())
    }

    pub async fn load_file(&mut self, file_path: &str) -> Result<(), OMCSessionZMQError> {
        let _ = self
            .send_expression(OMCExpression::LoadFile {
                file: file_path.to_string(),
            })
            .await?;
        Ok(())
    }

    pub async fn build_model(
        &mut self,
        model_name: &str,
    ) -> Result<BuildModelResults, OMCSessionZMQError> {
        let results = self
            .send_expression(OMCExpression::BuildModel {
                name: model_name.to_string(),
            })
            .await?;
        BuildModelResults::from_zmq(results).map_err(|e| e.into())
    }

    // TODO: Implement typed parser
    pub async fn send_expression(
        &mut self,
        expr: OMCExpression,
    ) -> Result<zeromq::ZmqMessage, OMCSessionZMQError> {
        trace!("Sending expression '{expr}' to OMC server.");
        self.socket.send(expr.to_string().into()).await?;
        let response = self.socket.recv().await?;
        trace!("Received response '{response:?}' from OMC request '{expr}'.");
        match expr {
            OMCExpression::Quit => Ok(response),
            _ => {
                self.socket
                    .send(OMCExpression::GetErrorString.to_string().into())
                    .await?;
                let error = self.socket.recv().await?;
                trace!("Received potential error: {:?}", error);
                let ret_err = match expr {
                    OMCExpression::BuildModel { .. } => {
                        // Check if `buildModelResults` exist
                        response.len() == 2
                    }
                    _ => {
                        // Check if response message is "true"
                        let cmd_successful = response
                            .get(0)
                            .and_then(|b| {
                                String::from_utf8(b.to_vec()).ok().map(|s| s.eq("true\n"))
                            })
                            .unwrap_or(false);
                        let is_error = error
                            .get(0)
                            .and_then(|b| String::from_utf8(b.to_vec()).ok())
                            .is_some();
                        is_error && !cmd_successful
                    }
                };
                if ret_err {
                    Err(OMCSessionZMQError::ExpressionError {
                        bytes: error.into_vec(),
                    })
                } else {
                    Ok(response)
                }
            }
        }
    }
}

impl Drop for OMCSessionZMQ {
    fn drop(&mut self) {
        trace!("Dropping OMC session ZMQ.");
        if path::Path::new(&self.port_file).exists() {
            trace!("Removing port file.");
            fs::remove_file(&self.port_file).unwrap();
        }
        trace!("Shutting OMC server down.");
        std::thread::scope(|s| {
            s.spawn(|| {
                let runtime = tokio::runtime::Builder::new_multi_thread().build().unwrap();
                match runtime.block_on(self.quit()) {
                    Ok(()) => {}
                    Err(e) => match e {
                        OMCSessionZMQError::ZmqErr(ZmqError::NoMessage) => {}
                        _ => error!("Failed to shut down OMC server. Error {e:?}"),
                    },
                }
            });
        });
    }
}

pub struct ModelicaSystem {
    model_file_path: path::PathBuf,
    model_name: String,
    xml_file_path: path::PathBuf,
    exec_file_path: path::PathBuf,
    simulation_options: HashMap<String, String>,
    result_file_path: path::PathBuf,
    current_user: String, // Only used to manage simulation log file(s)
    session_log_file_suffix: String, // Only used to manage simulation log file(s)
    session_random_string: String, // Only used to manage simulation log file(s)
}

impl ModelicaSystem {
    pub async fn new(
        session: &mut OMCSessionZMQ,
        file_path: &str,
        model_name: &str,
    ) -> Result<Self, ModelicaSystemError> {
        trace!("Creating new Modelica system [file_path={file_path}, model_name={model_name}].");
        Self::load_model(session, file_path).await?;
        let build_model_results = Self::build_model(session, model_name).await?;

        let exec_path = Path::new(&build_model_results.exec_path);
        let exec_dir = exec_path
            .parent()
            .ok_or_else(|| ModelicaSystemError::ExecParentDirError {
                exec_path: build_model_results.exec_path.clone(),
            })?
            .to_path_buf();
        let xml_file_path = exec_dir.join(build_model_results.xml_file_name);
        let modelica_system = ModelicaSystem {
            model_file_path: Path::new(file_path).to_path_buf(),
            model_name: model_name.to_string(),
            xml_file_path,
            exec_file_path: exec_path.to_path_buf(),
            simulation_options: HashMap::new(),
            result_file_path: Path::new(format!("{}_res.mat", model_name).as_str()).to_path_buf(),
            current_user: session.current_user.clone(),
            session_log_file_suffix: session.log_file_suffix.clone(),
            session_random_string: session.random_string.clone(),
        };
        // TODO: Parse XML
        Ok(modelica_system)
    }

    pub fn model_file_path(&self) -> &path::PathBuf {
        &self.model_file_path
    }

    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    pub fn xml_file_path(&self) -> &path::PathBuf {
        &self.xml_file_path
    }

    pub fn exec_file_path(&self) -> &path::PathBuf {
        &self.exec_file_path
    }

    pub fn result_file_path(&self) -> &path::PathBuf {
        &self.result_file_path
    }

    async fn load_model(
        session: &mut OMCSessionZMQ,
        model_file_path: &str,
    ) -> Result<(), ModelicaSystemError> {
        trace!("Loading model at '{model_file_path}'.");
        if path::Path::new(model_file_path).exists() {
            session
                .load_file(model_file_path)
                .await
                .map_err(|e| e.into())
        } else {
            Err(ModelicaSystemError::ModelNotFound {
                model_path: model_file_path.to_string(),
            })
        }
    }

    async fn build_model(
        session: &mut OMCSessionZMQ,
        model_name: &str,
    ) -> Result<BuildModelResults, ModelicaSystemError> {
        trace!("Building model '{model_name}'.");
        session.build_model(model_name).await.map_err(|e| e.into())
    }

    pub fn set_simulation_options(&mut self, opts: &[(&str, &str)]) {
        let mut out = String::new();
        let mut sep = "";
        for s in opts.iter().map(|(k, v)| format!("{k}={v}")) {
            out.push_str(sep);
            out.push_str(&*s);
            sep = ",";
        }
        trace!(
            "Setting simulation option(s) '{out}' for Modelica system '{}'.",
            &self.model_name
        );
        for (k, v) in opts {
            self.simulation_options.insert(k.to_string(), v.to_string());
        }
    }

    pub fn simulate(
        &mut self,
        result_file: Option<&str>,
        sim_flags: Option<&str>,
    ) -> Result<(ModelicaSimulation, oneshot::Receiver<()>), ModelicaSystemError> {
        let mut args: Vec<String> = Vec::new();
        if let Some(ref r) = result_file {
            self.result_file_path = Path::new(r).to_path_buf();
            args.push(format!("-r={r}"));
        };
        if let Some(s) = sim_flags {
            for split in s.split(' ') {
                args.push(split.to_string());
            }
        };
        let exec = &self.exec_file_path.to_str().ok_or_else(|| {
            OMCPathError::PathToStringConversionFailed {
                path: self.exec_file_path.clone(),
            }
        })?;

        if !self.simulation_options.is_empty() {
            let sim_opts = self
                .simulation_options
                .keys()
                .filter_map(|k| {
                    self.simulation_options
                        .get(k)
                        .map(|v| format!("{}={}", &**k, &**v))
                })
                .collect::<Vec<_>>()
                .join(",");
            let overr = format!("-override={sim_opts}");
            args.push(overr);
        }

        if self.exec_file_path.exists() {
            let log_file_path = format!(
                "/tmp/openmodelica.{}.{}.{}.sim.{}.{}.log",
                &self.current_user,
                &self.session_log_file_suffix,
                &self.session_random_string,
                &self.model_name,
                Uuid::new_v4()
            );
            let sim_log_file = fs::File::create(&log_file_path).map_err(|e| {
                ModelicaSystemError::FailedToCreateSimLogFile {
                    path: log_file_path.clone(),
                    source: e,
                }
            })?;
            let stdio = std_process::Stdio::from(sim_log_file);

            let args_str = args.join(" ");
            trace!(
                "Executing simulation: '{} {}'. Logging output at '{}'.",
                exec,
                args_str,
                log_file_path
            );
            let mut sim_process = process::Command::new(exec)
                .args(args)
                .stdout(stdio)
                .spawn()?;

            let cancellation_token = CancellationToken::new();
            let (completed_tx, completed_rx) = oneshot::channel::<()>();

            let model_name_clone = self.model_name.clone();
            let args_str_clone = args_str;
            tokio::spawn({
                let cancellation_token = cancellation_token.clone();
                async move {
                    select! {
                        _ = cancellation_token.cancelled() => {
                            // Simualtion cancelled
                            trace!("Simulation of model {} was cancelled [args={}].", model_name_clone, args_str_clone);
                            let _ = sim_process.kill().await;
                        }
                        _ = sim_process.wait() => {
                            // Simulation done
                            trace!("Simulation of model {} completed successfully [args={}].", model_name_clone, args_str_clone);
                            let _ = completed_tx.send(());
                        }
                    }
                }
            });

            Ok((ModelicaSimulation { cancellation_token }, completed_rx))
        } else {
            Err(ModelicaSystemError::SimNotFound {
                exec_path: exec.to_string(),
            })
        }
    }
}

pub struct ModelicaSimulation {
    cancellation_token: CancellationToken,
}

impl ModelicaSimulation {
    pub fn stop(&self) {
        self.cancellation_token.cancel();
    }
}

impl Drop for ModelicaSimulation {
    fn drop(&mut self) {
        self.cancellation_token.cancel();
    }
}
