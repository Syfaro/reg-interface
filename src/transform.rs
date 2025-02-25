use std::{collections::HashMap, io::Cursor};

use image::DynamicImage;
use jpeg2k::Image;
use rhai::{AST, Engine, Module, Scope};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tap::TapFallible;
use tracing::{debug, trace, warn};

use crate::{
    decoder::DecodedData,
    server::{ImageEntry, ImageStore},
};

#[derive(Clone, Debug, Deserialize)]
pub struct TransformConfig {
    template: Option<String>,
    template_path: Option<String>,
    extra: Option<rhai::Map>,
}

pub struct TransformManager {
    engine: Engine,
    ast: Option<AST>,
    extract_portraits: bool,
    image_store: ImageStore,
}

impl TransformManager {
    pub async fn new(
        config: TransformConfig,
        extract_portraits: bool,
        image_store: ImageStore,
    ) -> eyre::Result<Self> {
        let mut engine = Engine::new();
        engine.set_max_expr_depths(50, 50);
        engine.register_type_with_name::<crate::decoder::DecoderType>("DecoderType");

        let mut module = Module::new();
        for (name, value) in config.extra.clone().unwrap_or_default() {
            module.set_var(name, value);
        }
        engine.register_static_module("extra", module.into());

        let ast = Self::get_template_ast(&config, &engine).await?;

        Ok(Self {
            engine,
            ast,
            extract_portraits,
            image_store,
        })
    }

    pub fn transform(
        &self,
        data_context: &mut crate::decoder::DecodedDataContext,
    ) -> eyre::Result<(
        Option<serde_json::Value>,
        HashMap<String, serde_json::Value>,
    )> {
        let mut changes: HashMap<String, serde_json::Value> = Default::default();

        if self.extract_portraits {
            self.extract_mdl_portrait(&mut data_context.data)?;
        }

        let Some(ast) = self.ast.as_ref() else {
            return Ok((None, changes));
        };

        let mut scope = Scope::new();
        let mut return_value: rhai::Dynamic = self.engine.call_fn(
            &mut scope,
            ast,
            "transform",
            (
                data_context.input_name.clone(),
                data_context.decoder_type.to_string(),
                data_context.data.to_dynamic()?,
            ),
        )?;

        if return_value.is_unit() {
            return Ok((None, changes));
        }

        let mut cleaned_return_value = None;

        if let Ok(mut map) = return_value.as_map_mut() {
            if let Some(interface) = map.remove("_interface") {
                changes = rhai::serde::from_dynamic(&interface)?;

                if map.keys().all(|key| key == "_value") {
                    if let Some(value) = map.remove("_value") {
                        cleaned_return_value = Some(value);
                    }
                }
            }
        };

        Ok((
            Some(serde_json::to_value(
                cleaned_return_value.unwrap_or(return_value),
            )?),
            changes,
        ))
    }

    async fn get_template_ast(
        config: &TransformConfig,
        engine: &Engine,
    ) -> eyre::Result<Option<AST>> {
        let data = match (config.template_path.as_ref(), config.template.as_ref()) {
            (Some(path), _) => tokio::fs::read_to_string(&path).await?,
            (None, Some(data)) => data.clone(),
            (None, None) => return Ok(None),
        };

        let ast = engine.compile(data)?;
        eyre::ensure!(
            ast.iter_functions().any(|func| func.name == "transform"),
            "template must have transform function"
        );
        Ok(Some(ast))
    }

    fn extract_mdl_portrait(&self, data: &mut DecodedData) -> eyre::Result<()> {
        let DecodedData::Mdl(mdl) = data else {
            return Ok(());
        };

        if let Some(serde_json::Value::Array(portrait)) = mdl
            .response
            .get_mut("org.iso.18013.5.1")
            .and_then(|iso18013| iso18013.as_object_mut())
            .and_then(|iso18013| iso18013.remove("portrait"))
        {
            let data: Vec<u8> = portrait
                .into_iter()
                .filter_map(|value| value.as_i64())
                .filter_map(|value| u8::try_from(value).ok())
                .collect();
            let hash = hex::encode(Sha256::digest(&data));

            let im: Option<DynamicImage> = if infer::image::is_jpeg2000(&data) {
                trace!("got jpeg2000");
                Image::from_bytes(&data)
                    .tap_err(|err| warn!("could not decode jpeg2000: {err}"))
                    .ok()
                    .and_then(|im| (&im).try_into().ok())
            } else if infer::is_image(&data) {
                trace!("got other image format");
                image::load_from_memory(&data)
                    .tap_err(|err| warn!("could not decode image: {err}"))
                    .ok()
            } else {
                warn!("portrait data was not jpeg2000 or jpeg");
                None
            };

            if let Some(im) = im {
                debug!("adding portrait image: {hash}");
                let mut cursor = Cursor::new(Vec::new());
                im.write_to(&mut cursor, image::ImageFormat::Jpeg)?;
                mdl.response.insert(
                    "net.syfaro.reg-interface".to_string(),
                    serde_json::json!({
                        "portrait": hash,
                    }),
                );
                self.image_store.add(
                    hash,
                    ImageEntry {
                        content_type: "image/jpeg".to_string(),
                        single_use: true,
                        data: cursor.into_inner(),
                    },
                );
            }
        }

        Ok(())
    }
}
