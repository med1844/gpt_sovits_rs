use std::{collections::HashMap, str::FromStr, sync::Arc, usize};

use anyhow::Ok;
use tch::{IValue, Tensor};
use text::{g2p_en::G2PEnConverter, g2p_jp::G2PJpConverter, g2pw::G2PWConverter, CNBertModel};

pub mod symbols;
pub mod text;
pub use tch::Device;

pub struct GPTSovitsConfig {
    pub cn_setting: Option<(String, String)>,
    pub g2p_en_path: String,
    pub ssl_path: String,
    pub enable_jp: bool,
}

impl GPTSovitsConfig {
    pub fn new(ssl_path: String, g2p_en_path: String) -> Self {
        Self {
            cn_setting: None,
            g2p_en_path,
            ssl_path,
            enable_jp: false,
        }
    }

    pub fn with_chinese(mut self, g2pw_path: String, cn_bert_path: String) -> Self {
        self.cn_setting = Some((g2pw_path, cn_bert_path));
        self
    }

    pub fn with_jp(self, enable_jp: bool) -> Self {
        Self { enable_jp, ..self }
    }

    pub fn build(&self, device: Device) -> anyhow::Result<GPTSovits> {
        let (cn_bert, g2pw) = match &self.cn_setting {
            Some((g2pw_path, cn_bert_path)) => {
                let tokenizer = tokenizers::Tokenizer::from_str(text::g2pw::G2PW_TOKENIZER)
                    .map_err(|e| anyhow::anyhow!("load tokenizer error: {}", e))?;
                let tokenizer = Arc::new(tokenizer);

                let mut bert = tch::CModule::load_on_device(&cn_bert_path, device)?;
                bert.set_eval();

                let cn_bert_model = CNBertModel::new(Arc::new(bert), tokenizer.clone());
                let g2pw = G2PWConverter::new_with_device(g2pw_path, tokenizer.clone(), device)?;

                (cn_bert_model, g2pw)
            }
            _ => (CNBertModel::default(), G2PWConverter::empty()),
        };

        let mut ssl = tch::CModule::load_on_device(&self.ssl_path, device).unwrap();
        ssl.set_eval();

        Ok(GPTSovits {
            zh_bert: cn_bert,
            g2pw,
            g2p_en: G2PEnConverter::new(&self.g2p_en_path),
            g2p_jp: G2PJpConverter::new(),
            device,
            symbols: symbols::SYMBOLS.clone(),
            ssl,
            jieba: jieba_rs::Jieba::new(),
            speakers: HashMap::new(),
            enable_jp: self.enable_jp,
        })
    }
}

#[derive(Debug)]
pub struct Speaker {
    name: String,
    gpt_sovits: tch::CModule,
    ref_text: String,
    ssl_content: Tensor,
    ref_audio_32k: Tensor,
    ref_phone_seq: Tensor,
    ref_bert_seq: Tensor,
}

impl Speaker {
    pub fn get_name(&self) -> &str {
        &self.name
    }

    pub fn get_ref_text(&self) -> &str {
        &self.ref_text
    }

    pub fn get_ref_audio_32k(&self) -> &Tensor {
        &self.ref_audio_32k
    }

    pub fn infer(&self, text_phone_seq: &Tensor, bert_seq: &Tensor) -> anyhow::Result<Tensor> {
        let audio = self.gpt_sovits.forward_ts(&[
            &self.ssl_content,
            &self.ref_audio_32k,
            &self.ref_phone_seq,
            &text_phone_seq,
            &self.ref_bert_seq,
            &bert_seq,
        ])?;

        Ok(audio)
    }
}

pub struct GPTSovits {
    zh_bert: CNBertModel,
    g2pw: G2PWConverter,
    g2p_en: G2PEnConverter,
    g2p_jp: G2PJpConverter,
    device: tch::Device,
    symbols: HashMap<String, i64>,
    ssl: tch::CModule,

    speakers: HashMap<String, Speaker>,

    jieba: jieba_rs::Jieba,

    enable_jp: bool,
}

impl GPTSovits {
    pub fn new(
        zh_bert: CNBertModel,
        g2pw: G2PWConverter,
        g2p_en: G2PEnConverter,
        g2p_jp: G2PJpConverter,
        device: tch::Device,
        symbols: HashMap<String, i64>,
        ssl: tch::CModule,
        jieba: jieba_rs::Jieba,
        enable_jp: bool,
    ) -> Self {
        Self {
            zh_bert,
            g2pw,
            g2p_en,
            g2p_jp,
            device,
            symbols,
            speakers: HashMap::new(),
            ssl,
            jieba,
            enable_jp,
        }
    }

    pub fn create_speaker(
        &mut self,
        name: &str,
        gpt_sovits_path: &str,
        ref_audio_samples: &[f32],
        ref_audio_sr: usize,
        ref_text: &str,
    ) -> anyhow::Result<()> {
        tch::no_grad(|| {
            let mut gpt_sovits = tch::CModule::load_on_device(gpt_sovits_path, self.device)?;
            gpt_sovits.set_eval();

            // 避免句首吞字
            let ref_text = if !ref_text.ends_with(['。', '.']) {
                ref_text.to_string() + "."
            } else {
                ref_text.to_string()
            };

            let ref_audio = Tensor::from_slice(ref_audio_samples)
                .to_device(self.device)
                .unsqueeze(0);

            let ref_audio_16k = self.resample(&ref_audio, ref_audio_sr, 16000)?;
            let ref_audio_32k = self.resample(&ref_audio, ref_audio_sr, 32000)?;

            let ssl_content = self.ssl.forward_ts(&[&ref_audio_16k])?;

            let (ref_phone_seq, ref_bert_seq) = text::get_phone_and_bert(self, &ref_text)?;

            let speaker = Speaker {
                name: name.to_string(),
                gpt_sovits,
                ref_text,
                ssl_content,
                ref_audio_32k,
                ref_phone_seq,
                ref_bert_seq,
            };

            self.speakers.insert(name.to_string(), speaker);
            Ok(())
        })
    }

    pub fn resample(&self, audio: &Tensor, sr: usize, target_sr: usize) -> anyhow::Result<Tensor> {
        tch::no_grad(|| {
            let resample = self.ssl.method_is(
                "resample",
                &[
                    &IValue::Tensor(audio.shallow_clone()),
                    &IValue::Int(sr as i64),
                    &IValue::Int(target_sr as i64),
                ],
            )?;
            match resample {
                IValue::Tensor(resample) => Ok(resample),
                _ => unreachable!(),
            }
        })
    }

    /// generate a audio tensor from text
    pub fn infer(&self, speaker: &str, target_text: &str) -> anyhow::Result<Tensor> {
        log::debug!("start infer");
        tch::no_grad(|| {
            let speaker = self
                .speakers
                .get(speaker)
                .ok_or_else(|| anyhow::anyhow!("speaker not found"))?;

            let (phone_seq, bert_seq) = text::get_phone_and_bert(self, target_text)?;
            let audio = speaker.infer(&phone_seq, &bert_seq)?;
            Ok(audio)
        })
    }

    pub fn segment_infer(
        &self,
        speaker: &str,
        target_text: &str,
        split_chunk_size: usize,
    ) -> anyhow::Result<Tensor> {
        tch::no_grad(|| {
            let mut audios = vec![];
            let split_chunk_size = if split_chunk_size == 0 {
                50
            } else {
                split_chunk_size
            };
            let chunks = crate::text::split_text(target_text, split_chunk_size);
            log::debug!("segment_infer split_text result: {:#?}", chunks);
            for target_text in chunks {
                let audio = self.infer(speaker, target_text)?;
                audios.push(audio);
            }
            if !audios.is_empty() {
                Ok(Tensor::cat(&audios, 0))
            } else {
                Err(anyhow::anyhow!("no audio generated"))
            }
        })
    }
}
