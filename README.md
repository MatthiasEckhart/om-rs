<h1 align="center">om-rs</h1>

<p align="center">An <a href="https://openmodelica.org/">OpenModelica</a> Rust interface.</p>

## Why om-rs?

The purpose of this library is to provide an easy-to-use interface to <a href="https://openmodelica.org/">OpenModelica</a> in Rust.
Currently, its feature set is very limited.
However, in the future, similar functionality as <a href="https://github.com/OpenModelica/OMPython">OMPython</a> should be offered.
om-rs uses ZeroMQ to communicate with OpenModelica (i.e., CORBA is not supported).

## Getting started

### Add om-rs as a dependency

```toml
# Cargo.toml

[dependencies]
om-rs = { git = "https://github.com/MatthiasEckhart/om-rs", branch = "master" }
```

### Example

```rust
// main.rs

use om_rs;

#[tokio::main]
async fn main() {
    match om_rs::OMCSessionZMQ::new(None).await {
        Ok(mut session) => {
            match om_rs::ModelicaSystem::new(
                &mut session,
                "/home/user/Test.mo", // File path
                "Test", // Model name
            )
                .await
            {
                Ok(mut modelica_system) => {
                    modelica_system
                        .set_simulation_options(&[("stopTime", "1"), ("stepSize", "0.002")]);
                    match modelica_system.simulate(
                        Some("res.mat"), // Result file
                        Some("-w"),      // Simulation flags
                    ) {
                        Ok((_simulation, completed)) => {
                            let wait = tokio::spawn({
                                async move {
                                    // Wait for simulation process to stop
                                    let _ = completed.await;
                                    println!("Simulation finished.");
                                }
                            });

                            // Stop the simulation (for whatever reason)
                            //_simulation.stop();

                            // Wait until simulation is finished
                            match wait.await {
                                Ok(_) => println!("Bye."),
                                Err(e) => eprintln!("Failed with: {}", e),
                            }
                        }
                        Err(e) => eprintln!("Failed to run simulation: {}", e),
                    }
                }
                Err(e) => eprintln!("Failed to create Modelica system: {}", e),
            }
        }
        Err(e) => eprintln!("Failed to construct OMC session: {}", e),
    }
}
```


## Notes

om-rs is a research prototype and should not be used in production code.
At this state, it has very limited functionality and can only be used to build and simulate a model.
Furthermore, only Linux is supported at the moment.


