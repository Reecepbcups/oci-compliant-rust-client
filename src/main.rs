use reqwest::{Client, header};
use anyhow::{Result, Context, anyhow};
use serde_json::Value;
use std::env;
use std::fs;
use std::path::Path;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

#[tokio::main]
async fn main() -> Result<()> {
    // Get registry URL, namespace and package name from environment variables
    let registry = env::var("REGISTRY").context("REGISTRY environment variable not set")?;
    let namespace = env::var("PKG_NAMESPACE").context("PKG_NAMESPACE environment variable not set")?;
    let name = env::var("PKG_NAME").context("PKG_NAME environment variable not set")?;
    let version = env::var("PKG_VERSION").context("PKG_VERSION environment variable not set")?;

    // Create output directory
    let output_dir = format!("{}-{}", name, version);
    fs::create_dir_all(&output_dir).context("Failed to create output directory")?;

    // In OCI Distribution Spec, the correct URL format is:
    // /v2/<name>/manifests/<reference>
    // Where <name> is the repository name (namespace/package)
    let url = format!("http://{}/v2/{}/{}/manifests/{}",
        registry,
        namespace,
        name,
        version);

    println!("Fetching manifest from: {}", url);

    // Create a client with appropriate headers for OCI registry
    let client = Client::new();

    // Create request with proper Accept headers for OCI manifest
    // Include multiple acceptable formats including the WASM config type
    let response = client.get(&url)
        .header(header::ACCEPT, "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.wasm.config.v0+json")
        .send()
        .await
        .context("Failed to send request")?;

    // Debug information
    println!("Response status: {}", response.status());
    println!("Response headers: {:#?}", response.headers());

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_else(|_| "Could not get response text".to_string());

        // Provide some debugging guidance
        println!("\nDEBUG INFO:");
        println!("1. If you got a 404, check if the registry implements the OCI Distribution Spec");
        println!("2. Try listing repositories with: GET /v2/_catalog");
        println!("3. Try listing tags with: GET /v2/{}/{}/tags/list", namespace, name);

        // Try the catalog endpoint to see what's available
        println!("\nAttempting to list available repositories...");
        let catalog_url = format!("http://{}/v2/_catalog", registry);
        match client.get(&catalog_url).send().await {
            Ok(catalog_resp) => {
                if catalog_resp.status().is_success() {
                    let catalog: Value = catalog_resp.json().await?;
                    println!("Available repositories: {}", serde_json::to_string_pretty(&catalog)?);
                } else {
                    println!("Failed to list repositories: {}", catalog_resp.status());
                }
            },
            Err(e) => println!("Error listing repositories: {}", e),
        }

        return Err(anyhow!("HTTP error {}: {}", status, text));
    }

    // Parse the JSON response
    let manifest: Value = response.json().await.context("Failed to parse manifest JSON")?;

    // Save the manifest
    let manifest_path = format!("{}/manifest.json", output_dir);
    fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)
        .context("Failed to save manifest")?;

    println!("Saved manifest to {}", manifest_path);

    // For WASM artifacts, we care most about the layers which contain the WASM modules
    if let Some(layers) = manifest.get("layers").and_then(|l| l.as_array()) {
        println!("\nFound {} layer(s) in the manifest", layers.len());

        for (i, layer) in layers.iter().enumerate() {
            if let Some(digest) = layer.get("digest").and_then(|d| d.as_str()) {
                // Check if this is a WASM file based on mediaType
                let is_wasm = layer.get("mediaType")
                    .and_then(|m| m.as_str())
                    .map(|m| m == "application/wasm")
                    .unwrap_or(false);

                // Get the filename from annotations if available
                let filename = layer.get("annotations")
                    .and_then(|a| a.get("org.opencontainers.image.title"))
                    .and_then(|t| t.as_str())
                    .map(|s| Path::new(s).file_name().and_then(|f| f.to_str()).unwrap_or(s))
                    .unwrap_or_else(|| {
                        let s = if is_wasm {
                            format!("module_{}.wasm", i)
                        } else {
                            format!("blob_{}", i)
                        };
                        Box::leak(s.into_boxed_str())
                    });

                println!("Downloading layer {}: {} ({})",
                         i,
                         filename,
                         if is_wasm { "WASM module" } else { "other content" });

                // Download the blob
                download_blob(
                    &client,
                    &registry,
                    &namespace,
                    &name,
                    digest,
                    &filename,
                    &output_dir
                ).await?;
            }
        }
    } else {
        println!("No layers found in the manifest. Checking for other content references...");

        // If we can't find structured layers, this might be a custom OCI artifact
        // Try to download the config blob if present
        if let Some(config) = manifest.get("config") {
            if let Some(digest) = config.get("digest").and_then(|d| d.as_str()) {
                let config_type = config.get("mediaType")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown");

                println!("Downloading config blob ({}): {}", config_type, digest);
                download_blob(&client, &registry, &namespace, &name, digest, "config.json", &output_dir).await?;
            }
        }
    }

    println!("\nContent downloaded successfully to {}", output_dir);
    Ok(())
}

async fn download_blob(
    client: &Client,
    registry: &str,
    namespace: &str,
    name: &str,
    digest: &str,
    output_filename: &str,
    output_dir: &str
) -> Result<()> {
    let blob_url = format!("http://{}/v2/{}/{}/blobs/{}",
        registry, namespace, name, digest);

    println!("  Fetching from: {}", blob_url);

    let resp = client.get(&blob_url)
        .send()
        .await
        .with_context(|| format!("Failed to download blob: {}", digest))?;

    if !resp.status().is_success() {
        return Err(anyhow!("Failed to download blob {}: {}", digest, resp.status()));
    }

    let output_path = format!("{}/{}", output_dir, output_filename);
    let bytes = resp.bytes().await?;

    // Save the blob content
    let mut file = File::create(&output_path).await?;
    file.write_all(&bytes).await?;

    println!("  Saved to {} ({} bytes)", output_path, bytes.len());

    // If it seems to be JSON, also save a pretty version
    if output_filename.ends_with(".json") || bytes.len() > 0 && bytes[0] == b'{' {
        if let Ok(json_str) = String::from_utf8(bytes.to_vec()) {
            if let Ok(json_value) = serde_json::from_str::<Value>(&json_str) {
                let pretty_path = format!("{}/{}_pretty.json", output_dir,
                    output_filename.trim_end_matches(".json"));
                fs::write(&pretty_path, serde_json::to_string_pretty(&json_value)?)?;
                println!("  Also saved pretty JSON to {}", pretty_path);
            }
        }
    }

    Ok(())
}
