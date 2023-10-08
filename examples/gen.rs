use anyhow::Result;
use clap::Parser;
#[cfg(not(debug_assertions))]
use dialoguer::{theme::ColorfulTheme, Select};
use itertools::Itertools;
use memmap2::Mmap;
use std::{
    fs::File,
    io::{BufReader, Read, Write},
    path::PathBuf,
    time::{Duration, Instant},
};
use web_rwkv::{
    context::{Context, ContextBuilder, Instance},
    model::{
        loader::Loader, v4, v5, LayerFlags, Lora, Model, ModelBuilder, ModelState, ModelVersion,
        Quantization, StateBuilder,
    },
    tokenizer::Tokenizer,
};

fn sample(probs: &[f32], top_p: f32) -> u16 {
    let sorted = probs
        .iter()
        .copied()
        .enumerate()
        .sorted_unstable_by(|(_, x), (_, y)| x.total_cmp(y).reverse())
        .scan((0, 0.0), |(_, cum), (id, x)| {
            if *cum > top_p {
                None
            } else {
                *cum += x;
                Some((id, *cum))
            }
        })
        .collect_vec();
    let sum: f32 = sorted.iter().map(|(_, x)| x).sum();
    let sorted = sorted.into_iter().map(|(id, x)| (id, x / sum));

    let rand = fastrand::f32();
    let token = sorted
        .into_iter()
        .find_or_first(|&(_, cum)| rand <= cum)
        .map(|(id, _)| id)
        .unwrap_or_default();
    token as u16
}

async fn create_context() -> Result<Context> {
    let instance = Instance::new();
    #[cfg(not(debug_assertions))]
    let adapter = {
        let adapters = instance.adapters();
        let selection = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Please select an adapter")
            .default(0)
            .items(&adapters)
            .interact()?;
        instance.select_adapter(selection)?
    };
    #[cfg(debug_assertions)]
    let adapter = instance
        .adapter(wgpu::PowerPreference::HighPerformance)
        .await?;
    let context = ContextBuilder::new(adapter)
        .with_default_pipelines()
        .with_quant_pipelines()
        .build()
        .await?;
    println!("{:#?}", context.adapter.get_info());
    Ok(context)
}

fn load_tokenizer() -> Result<Tokenizer> {
    let file = File::open("assets/rwkv_vocab_v20230424.json")?;
    let mut reader = BufReader::new(file);
    let mut contents = String::new();
    reader.read_to_string(&mut contents)?;
    Ok(Tokenizer::new(&contents)?)
}

fn load_model<M: Model>(
    context: &Context,
    data: &[u8],
    lora: Option<PathBuf>,
    quant: Option<u64>,
) -> Result<M> {
    let quant = quant
        .map(|bits| Quantization::Int8(LayerFlags::from_bits_retain(bits)))
        .unwrap_or_default();
    let model = ModelBuilder::new(context, data).with_quant(quant);
    match lora {
        Some(lora) => {
            let file = File::open(lora)?;
            let map = unsafe { Mmap::map(&file)? };
            model
                .add_lora(Lora {
                    data: &map,
                    blend: Default::default(),
                })
                .build()
        }
        None => model.build(),
    }
}

async fn run(cli: Cli) -> Result<()> {
    let context = create_context().await?;

    let tokenizer = load_tokenizer()?;
    let model = cli.model.unwrap_or(
        std::fs::read_dir("assets/models")
            .unwrap()
            .filter_map(|x| x.ok())
            .find(|x| x.path().extension().is_some_and(|x| x == "st"))
            .unwrap()
            .path(),
    );

    let file = File::open(model)?;
    let map = unsafe { Mmap::map(&file)? };

    let info = Loader::info(&map)?;
    println!("{:#?}", info);

    match info.version {
        ModelVersion::V4 => {
            let model: v4::Model = load_model(&context, &map, cli.lora, cli.quant)?;
            let state: v4::ModelState = StateBuilder::new(&context, model.info()).build();
            run_internal(model, state, tokenizer)
        }
        ModelVersion::V5 => {
            let model: v5::Model = load_model(&context, &map, cli.lora, cli.quant)?;
            let state: v5::ModelState = StateBuilder::new(&context, model.info()).build();
            run_internal(model, state, tokenizer)
        }
    }
}

fn run_internal<M, S>(model: M, state: S, tokenizer: Tokenizer) -> Result<()>
where
    S: ModelState,
    M: Model<ModelState = S>,
{
    let prompt = "The Eiffel Tower is located in the city of";
    let mut tokens = vec![tokenizer.encode(prompt.as_bytes())?];
    print!("{}", prompt);
    let mut instant;
    let mut duration = Duration::default();

    let num_tokens = 100;
    for index in 0..=num_tokens {
        instant = Instant::now();
        let logits = model.run(&mut tokens, &state)?;
        let probs = model.softmax(logits)?;
        duration = match index {
            0 => Duration::default(),
            _ => duration + instant.elapsed(),
        };

        if let Some(probs) = &probs[0] {
            let token = sample(probs, 0.5);
            let word = String::from_utf8(tokenizer.decode(&[token])?)?;
            print!("{}", word);
            tokens[0] = vec![token];
        }
    }

    println!("\n{} tokens: {} mills", num_tokens, duration.as_millis());
    std::io::stdout().flush()?;

    Ok(())
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[arg(short, long, value_name = "FILE")]
    model: Option<PathBuf>,
    #[arg(short, long, value_name = "FILE")]
    lora: Option<PathBuf>,
    #[arg(short, long, value_name = "LAYERS")]
    quant: Option<u64>,
}

fn main() {
    let cli = Cli::parse();
    pollster::block_on(run(cli)).unwrap();
}
