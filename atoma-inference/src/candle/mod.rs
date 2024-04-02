pub mod stable_diffusion;
pub mod token_output_stream;

use std::{fs::File, io::Write, path::PathBuf};

use candle::{
    utils::{cuda_is_available, metal_is_available},
    DType, Device, Tensor,
};
use tracing::info;

use crate::models::ModelError;

pub trait CandleModel {
    type Fetch;
    type Input;
    fn fetch(fetch: &Self::Fetch) -> Result<(), ModelError>;
    fn inference(input: Self::Input) -> Result<Vec<Tensor>, ModelError>;
}

pub fn device() -> Result<Device, candle::Error> {
    if cuda_is_available() {
        info!("Using CUDA");
        Device::new_cuda(0)
    } else if metal_is_available() {
        info!("Using Metal");
        Device::new_metal(0)
    } else {
        info!("Using Cpu");
        Ok(Device::Cpu)
    }
}

pub fn hub_load_safetensors(
    repo: &hf_hub::api::sync::ApiRepo,
    json_file: &str,
) -> candle::Result<Vec<std::path::PathBuf>> {
    let json_file = repo.get(json_file).map_err(candle::Error::wrap)?;
    let json_file = std::fs::File::open(json_file)?;
    let json: serde_json::Value =
        serde_json::from_reader(&json_file).map_err(candle::Error::wrap)?;
    let weight_map = match json.get("weight_map") {
        None => candle::bail!("no weight map in {json_file:?}"),
        Some(serde_json::Value::Object(map)) => map,
        Some(_) => candle::bail!("weight map in {json_file:?} is not a map"),
    };
    let mut safetensors_files = std::collections::HashSet::new();
    for value in weight_map.values() {
        if let Some(file) = value.as_str() {
            safetensors_files.insert(file.to_string());
        }
    }
    let safetensors_files = safetensors_files
        .iter()
        .map(|v| repo.get(v).map_err(candle::Error::wrap))
        .collect::<candle::Result<Vec<_>>>()?;
    Ok(safetensors_files)
}

pub fn save_image<P: AsRef<std::path::Path>>(img: &Tensor, p: P) -> candle::Result<()> {
    let p = p.as_ref();
    let (channel, height, width) = img.dims3()?;
    if channel != 3 {
        candle::bail!("save_image expects an input of shape (3, height, width)")
    }
    let img = img.permute((1, 2, 0))?.flatten_all()?;
    let pixels = img.to_vec1::<u8>()?;
    let image: image::ImageBuffer<image::Rgb<u8>, Vec<u8>> =
        match image::ImageBuffer::from_raw(width as u32, height as u32, pixels) {
            Some(image) => image,
            None => candle::bail!("error saving image {p:?}"),
        };
    image.save(p).map_err(candle::Error::wrap)?;
    Ok(())
}

pub fn save_tensor_to_file(tensor: &Tensor, filename: &str) -> Result<(), candle::Error> {
    let json_output = serde_json::to_string(
        &tensor
            .to_device(&Device::Cpu)?
            .flatten_all()?
            .to_dtype(DType::F64)?
            .to_vec1::<f64>()?,
    )
    .unwrap();
    let mut file = File::create(PathBuf::from(filename))?;
    file.write_all(json_output.as_bytes())?;
    Ok(())
}
