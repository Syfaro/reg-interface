use std::collections::HashMap;

use rhai::{AST, Engine, Module, Scope};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct TransformConfig {
    template: Option<String>,
    template_path: Option<String>,
    extra: Option<rhai::Map>,
}

pub struct TransformManager {
    engine: Engine,
    ast: Option<AST>,
}

impl TransformManager {
    pub async fn new(config: TransformConfig) -> eyre::Result<Self> {
        let mut engine = Engine::new();
        engine.set_max_expr_depths(50, 50);
        engine.register_type_with_name::<crate::decoder::DecoderType>("DecoderType");

        let mut module = Module::new();
        for (name, value) in config.extra.clone().unwrap_or_default() {
            module.set_var(name, value);
        }
        engine.register_static_module("extra", module.into());

        let ast = Self::get_template_ast(&config, &engine).await?;

        Ok(Self { engine, ast })
    }

    pub fn transform(
        &self,
        data_context: &crate::decoder::DecodedDataContext,
    ) -> eyre::Result<(
        Option<serde_json::Value>,
        HashMap<String, serde_json::Value>,
    )> {
        let mut changes: HashMap<String, serde_json::Value> = Default::default();

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
}
