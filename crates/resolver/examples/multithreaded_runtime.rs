#![recursion_limit = "128"]

//! This example shows how to create a resolver that uses the tokio multithreaded runtime. This is how
//! you might integrate the resolver into a more complex application.

fn main() {
    tracing_subscriber::fmt::init();
    run();
}

fn run() {
    use tokio::runtime::Runtime;

    // Set up the standard tokio runtime (multithreaded by default).
    let runtime = Runtime::new().expect("Failed to create runtime");

    let resolver = {
        // To make this independent, if targeting macOS, BSD, Linux, or Windows, we can use the system's configuration:
        #[cfg(any(unix, windows))]
        {
            use hickory_resolver::{TokioResolver, proto::runtime::TokioRuntimeProvider};

            // use the system resolver configuration
            TokioResolver::builder(TokioRuntimeProvider::default())
                .expect("failed to create resolver")
                .build()
                .unwrap()
        }

        // For other operating systems, we can use one of the preconfigured definitions
        #[cfg(not(any(unix, windows)))]
        {
            // Directly reference the config types
            use hickory_resolver::{
                Resolver,
                config::{GOOGLE, ResolverConfig, ResolverOpts},
            };

            // Get a new resolver with the google nameservers as the upstream recursive resolvers
            Resolver::tokio(ResolverConfig::udp_and_tcp(), ResolverOpts::default())
        }
    };

    // Create some futures representing name lookups.
    let names = &["www.google.com", "www.reddit.com", "www.wikipedia.org"];
    let mut futures = names
        .iter()
        .map(|name| (name, resolver.lookup_ip(*name)))
        .collect::<Vec<_>>();

    // Go through the list of resolution operations and wait for them to complete.
    for (name, lookup) in futures.drain(..) {
        let ips = runtime
            .block_on(lookup)
            .expect("Failed completing lookup future")
            .iter()
            .collect::<Vec<_>>();
        println!("{name} resolved to {ips:?}");
    }

    // Drop the resolver, which means that the runtime will become idle.
    drop(futures);
    drop(resolver);
}

#[test]
fn test_multithreaded_runtime() {
    test_support::subscribe();
    run()
}
