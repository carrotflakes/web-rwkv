use anyhow::Result;
use derive_getters::Getters;
use half::f16;
use itertools::Itertools;
use safetensors::SafeTensors;
use wgpu::{CommandEncoderDescriptor, ComputePassDescriptor};

use super::{Lora, ModelInfo, ModelVersion};
use crate::{
    context::Context,
    tensor::{
        ops::{TensorOp, TensorPass},
        shape::{Shape, TensorDimension},
        ReadWrite, TensorCpu, TensorGpu, TensorInit, TensorReshape, TensorShape,
    },
};

#[derive(Getters)]
pub struct Loader<'a> {
    context: Context,
    model: SafeTensors<'a>,
    lora: Vec<Lora>,
}

struct LoraVector {
    tensor: TensorGpu<f32, ReadWrite>,
    alpha: f32,
}

struct LoraMatrix {
    a: TensorGpu<f16, ReadWrite>,
    b: TensorGpu<f16, ReadWrite>,
    rank: usize,
    alpha: f32,
}

impl<'a> Loader<'a> {
    pub fn new(context: &Context, data: &'a [u8], lora: Vec<Lora>) -> Result<Loader<'a>> {
        let model = SafeTensors::deserialize(data)?;
        let lora = lora
            .into_iter()
            .map(|lora| -> Result<_> {
                let _ = SafeTensors::deserialize(&lora.data)?;
                Ok(lora)
            })
            .try_collect()?;
        Ok(Self {
            context: context.clone(),
            model,
            lora,
        })
    }

    pub fn info(data: &'a [u8]) -> Result<ModelInfo> {
        let model = SafeTensors::deserialize(data)?;
        let num_layers = {
            let mut r: usize = 0;
            for i in model.names() {
                const PREFIX: &str = "blocks.";
                if let Some(i) = i.strip_prefix(PREFIX) {
                    let i = &i[..i.find('.').unwrap_or(0)];
                    r = r.max(i.parse::<usize>()?)
                }
            }
            r + 1
        };

        let embed = model.tensor("emb.weight")?;
        let ffn = model.tensor("blocks.0.ffn.key.weight")?;
        let time_decay = model.tensor("blocks.0.att.time_decay")?;
        let version = match model.tensor("blocks.0.att.gate.weight") {
            Ok(_) => ModelVersion::V5,
            Err(_) => ModelVersion::V4,
        };

        let num_emb = embed.shape()[1];
        let num_hidden = ffn.shape()[0];
        let num_vocab = embed.shape()[0];
        let num_head = time_decay.shape()[0];

        Ok(ModelInfo {
            version,
            num_layer: num_layers,
            num_emb,
            num_hidden,
            num_vocab,
            num_head,
        })
    }

    /// Load all lora and blend factors about the vector with a given name.
    /// In each LoRA, only the last matched pattern is loaded.
    fn lora_vectors(&self, name: impl AsRef<str>) -> Vec<LoraVector> {
        let name = name.as_ref();
        self.lora
            .iter()
            .filter_map(|lora| {
                let data = SafeTensors::deserialize(&lora.data).ok()?;
                lora.blend
                    .iter()
                    .filter(|blend| blend.pattern.is_match(name))
                    .last()
                    .and_then(|blend| {
                        data.tensor(name).ok().and_then(|tensor| {
                            let tensor = TensorCpu::<f16>::from_safetensors(&self.context, tensor)
                                .ok()?
                                .map(|x| x.to_f32())
                                .into();
                            let alpha = blend.alpha;
                            log::info!("loaded lora {}, alpha: {}", name, alpha);
                            Some(LoraVector { tensor, alpha })
                        })
                    })
            })
            .collect()
    }

    /// Load all lora and blend factors about the matrix with a given name.
    /// In each LoRA, only the last matched pattern is loaded.
    fn lora_matrices(&self, name: impl AsRef<str>) -> Vec<LoraMatrix> {
        let name = name.as_ref();
        self.lora
            .iter()
            .filter_map(|lora| {
                let data = SafeTensors::deserialize(&lora.data).ok()?;
                lora.blend
                    .iter()
                    .filter(|blend| blend.pattern.is_match(name))
                    .last()
                    .and_then(|blend| {
                        let context = &self.context;

                        let a = data
                            .tensor(&format!("{name}.lora.0"))
                            .ok()
                            .and_then(|tensor| TensorGpu::from_safetensors(context, tensor).ok())?;
                        let b = data
                            .tensor(&format!("{name}.lora.1"))
                            .ok()
                            .and_then(|tensor| TensorGpu::from_safetensors(context, tensor).ok())?;
                        // let tensor =
                        //     TensorGpu::init(context, Shape::new(a.shape()[1], b.shape()[1], 1, 1));

                        // let mut encoder = context
                        //     .device
                        //     .create_command_encoder(&CommandEncoderDescriptor::default());

                        // let op = TensorOp::matmul_mat_fp16(
                        //     b.view(.., .., .., ..).ok()?,
                        //     a.view(.., .., .., ..).ok()?,
                        //     tensor.view(.., .., .., ..).ok()?,
                        // )
                        // .ok()?;
                        // let mut pass =
                        //     encoder.begin_compute_pass(&ComputePassDescriptor::default());
                        // pass.execute_tensor_op(&op);
                        // drop(pass);

                        // context.queue.submit(Some(encoder.finish()));

                        let rank = a.shape()[0];
                        let alpha = blend.alpha;

                        log::info!("loaded lora {}, alpha: {}", name, blend.alpha);
                        Some(LoraMatrix { a, b, rank, alpha })
                    })
            })
            .collect()
    }

    pub fn load_vector_f32(&self, name: impl AsRef<str>) -> Result<TensorGpu<f32, ReadWrite>> {
        use TensorDimension::{Auto, Dimension};
        let tensor = self.model.tensor(name.as_ref())?;
        let tensor = TensorCpu::<f16>::from_safetensors(&self.context, tensor)?
            .map(|x| x.to_f32())
            .reshape(Auto, Dimension(1), Dimension(1), Dimension(1))?
            .into();

        let mut encoder = self
            .context
            .device
            .create_command_encoder(&CommandEncoderDescriptor::default());

        for lora in self.lora_vectors(name) {
            let factor = vec![lora.alpha, 1.0 - lora.alpha, 0.0, 0.0];
            let factor = TensorGpu::from_data(&self.context, Shape::new(4, 1, 1, 1), &factor)?;
            let op = TensorOp::blend(&factor, &lora.tensor, &tensor)?;
            let mut pass = encoder.begin_compute_pass(&ComputePassDescriptor::default());
            pass.execute_tensor_op(&op);
        }

        self.context.queue.submit(Some(encoder.finish()));
        Ok(tensor)
    }

    pub fn load_vector_exp_f32(&self, name: impl AsRef<str>) -> Result<TensorGpu<f32, ReadWrite>> {
        use TensorDimension::{Auto, Dimension};
        let tensor = self.model.tensor(name.as_ref())?;
        let tensor = TensorCpu::<f16>::from_safetensors(&self.context, tensor)?
            .map(|x| -x.to_f32().exp())
            .reshape(Auto, Dimension(1), Dimension(1), Dimension(1))?
            .into();

        let mut encoder = self
            .context
            .device
            .create_command_encoder(&CommandEncoderDescriptor::default());

        for lora in self.lora_vectors(name) {
            let factor = vec![lora.alpha, 1.0 - lora.alpha, 0.0, 0.0];
            let factor = TensorGpu::from_data(&self.context, Shape::new(4, 1, 1, 1), &factor)?;
            let op = TensorOp::blend(&factor, &lora.tensor, &tensor)?;
            let mut pass = encoder.begin_compute_pass(&ComputePassDescriptor::default());
            pass.execute_tensor_op(&op);
        }

        self.context.queue.submit(Some(encoder.finish()));
        Ok(tensor)
    }

    pub fn load_vector_exp_exp_f32(
        &self,
        name: impl AsRef<str>,
    ) -> Result<TensorGpu<f32, ReadWrite>> {
        use TensorDimension::{Auto, Dimension};
        let tensor = self.model.tensor(name.as_ref())?;
        let tensor = TensorCpu::<f16>::from_safetensors(&self.context, tensor)?
            .map(|x| -x.to_f32().exp())
            .map(|x| x.exp())
            .reshape(Auto, Dimension(1), Dimension(1), Dimension(1))?
            .into();

        let mut encoder = self
            .context
            .device
            .create_command_encoder(&CommandEncoderDescriptor::default());

        for lora in self.lora_vectors(name) {
            let factor = vec![lora.alpha, 1.0 - lora.alpha, 0.0, 0.0];
            let factor = TensorGpu::from_data(&self.context, Shape::new(4, 1, 1, 1), &factor)?;
            let op = TensorOp::blend(&factor, &lora.tensor, &tensor)?;
            let mut pass = encoder.begin_compute_pass(&ComputePassDescriptor::default());
            pass.execute_tensor_op(&op);
        }

        self.context.queue.submit(Some(encoder.finish()));
        Ok(tensor)
    }

    pub fn load_vector_f16(&self, name: impl AsRef<str>) -> Result<TensorGpu<f16, ReadWrite>> {
        use TensorDimension::{Auto, Dimension};
        let context = &self.context;
        let lora = self.lora_vectors(name.as_ref());
        let tensor = self.model.tensor(name.as_ref())?;
        let tensor = if lora.is_empty() {
            TensorGpu::from_safetensors(context, tensor)?.reshape(
                Auto,
                Dimension(1),
                Dimension(1),
                Dimension(1),
            )?
        } else {
            let tensor_f32 = TensorCpu::<f16>::from_safetensors(context, tensor)?
                .map(|x| x.to_f32())
                .reshape(Auto, Dimension(1), Dimension(1), Dimension(1))?;
            let tensor_f32 = TensorGpu::from(tensor_f32);
            let tensor_f16 = context.tensor_init(tensor_f32.shape());

            let mut encoder = context
                .device
                .create_command_encoder(&CommandEncoderDescriptor::default());

            for lora in lora {
                let factor = vec![lora.alpha, 1.0 - lora.alpha, 0.0, 0.0];
                let factor = TensorGpu::from_data(context, Shape::new(4, 1, 1, 1), &factor)?;
                let op = TensorOp::blend(&factor, &lora.tensor, &tensor_f32)?;
                let mut pass = encoder.begin_compute_pass(&ComputePassDescriptor::default());
                pass.execute_tensor_op(&op);
            }

            let op = TensorOp::quantize_fp16(&tensor_f32, &tensor_f16)?;
            let mut pass = encoder.begin_compute_pass(&ComputePassDescriptor::default());
            pass.execute_tensor_op(&op);
            drop(pass);

            context.queue.submit(Some(encoder.finish()));
            tensor_f16
        };
        Ok(tensor)
    }

    pub fn load_matrix_f16(&self, name: impl AsRef<str>) -> Result<TensorGpu<f16, ReadWrite>> {
        use TensorDimension::{Dimension, Full};
        let context = &self.context;
        let lora = self.lora_matrices(name.as_ref());
        let tensor = self.model.tensor(name.as_ref())?;
        let tensor = if lora.is_empty() {
            TensorGpu::from_safetensors(context, tensor)?.reshape(
                Full,
                Full,
                Dimension(1),
                Dimension(1),
            )?
        } else {
            let tensor = TensorGpu::from_safetensors(context, tensor)?.reshape(
                Full,
                Full,
                Dimension(1),
                Dimension(1),
            )?;

            let mut encoder = context
                .device
                .create_command_encoder(&CommandEncoderDescriptor::default());
            for lora in lora {
                let factor = vec![lora.alpha / lora.rank as f32, 1.0, 0.0, 0.0];
                let factor = TensorGpu::from_data(context, Shape::new(4, 1, 1, 1), &factor)?;
                let ops = TensorOp::List(vec![TensorOp::blend_lora(
                    &factor,
                    lora.b.view(.., .., .., ..)?,
                    lora.a.view(.., .., .., ..)?,
                    tensor.view(.., .., .., ..)?,
                )?]);
                let mut pass = encoder.begin_compute_pass(&ComputePassDescriptor::default());
                pass.execute_tensor_op(&ops);
            }
            context.queue.submit(Some(encoder.finish()));
            tensor
        };
        Ok(tensor)
    }

    pub fn load_matrix_f16_discount(
        &self,
        name: impl AsRef<str>,
        discount: f32,
    ) -> Result<TensorGpu<f16, ReadWrite>> {
        use TensorDimension::{Dimension, Full};
        let context = &self.context;

        let lora = self.lora_matrices(name.as_ref());
        let tensor = self.model.tensor(name.as_ref())?;
        let tensor = if lora.is_empty() {
            let tensor = TensorCpu::<f16>::from_safetensors(context, tensor)?
                .map(|&x| f16::from_f32(discount * x.to_f32()))
                .reshape(Full, Full, Dimension(1), Dimension(1))?;
            TensorGpu::from(tensor)
        } else {
            let tensor = TensorCpu::<f16>::from_safetensors(context, tensor)?
                .map(|x| f16::from_f32(discount * x.to_f32()))
                .reshape(Full, Full, Dimension(1), Dimension(1))?;
            let tensor = TensorGpu::from(tensor);

            let mut encoder = context
                .device
                .create_command_encoder(&CommandEncoderDescriptor::default());
            for lora in lora {
                let factor = vec![lora.alpha / lora.rank as f32, 1.0, 0.0, 0.0];
                let factor = TensorGpu::from_data(context, Shape::new(4, 1, 1, 1), &factor)?;
                let ops = TensorOp::List(vec![TensorOp::blend_lora(
                    &factor,
                    lora.b.view(.., .., .., ..)?,
                    lora.a.view(.., .., .., ..)?,
                    tensor.view(.., .., .., ..)?,
                )?]);
                let mut pass = encoder.begin_compute_pass(&ComputePassDescriptor::default());
                pass.execute_tensor_op(&ops);
            }
            context.queue.submit(Some(encoder.finish()));
            tensor
        };
        Ok(tensor)
    }

    pub fn load_embed<'b>(&self) -> Result<TensorCpu<'b, f16>> {
        let embed = self.model.tensor("emb.weight")?;
        let num_emb = embed.shape()[1];
        let num_vocab = embed.shape()[0];
        let tensor = self.context.tensor_from_data(
            Shape::new(num_emb, num_vocab, 1, 1),
            bytemuck::pod_collect_to_vec(embed.data()),
        )?;
        Ok(tensor)
    }

    pub fn load_head(&self, chunk_size: usize) -> Result<Vec<TensorGpu<f16, ReadWrite>>> {
        let context = &self.context;
        let tensor = self.model.tensor("head.weight")?;
        let shape = tensor.shape();
        let shape = Shape::new(shape[1], shape[0], 1, 1);
        let chunks = shape[1] / chunk_size;
        let data = bytemuck::cast_slice(tensor.data());

        let head = (0..chunks)
            .map(|chunk| {
                let start = (chunk * chunk_size) * shape[0];
                let end = start + chunk_size * shape[0];
                context.tensor_from_data(Shape::new(shape[0], chunk_size, 1, 1), &data[start..end])
            })
            .try_collect()?;
        Ok(head)
    }
}
