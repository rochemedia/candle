use crate::nn::{Conv1D, ConvConfig, VarBuilder};
use anyhow::Result;
use candle::Tensor;

// Encodec Model
// https://github.com/huggingface/transformers/blob/main/src/transformers/models/encodec/modeling_encodec.py

#[derive(Debug, Clone, PartialEq)]
enum NormType {
    WeightNorm,
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    target_bandwidths: Vec<f64>,
    sampling_rate: usize,
    audio_channels: usize,
    normalize: bool,
    chunk_length_s: Option<usize>,
    overlap: Option<usize>,
    hidden_size: usize,
    num_filters: usize,
    num_residual_layers: usize,
    upsampling_ratios: Vec<usize>,
    norm_type: NormType,
    kernel_size: usize,
    last_kernel_size: usize,
    residual_kernel_size: usize,
    dilation_growth_rate: usize,
    use_causal_conv: bool,
    pad_mode: &'static str,
    compress: usize,
    num_lstm_layers: usize,
    trim_right_ratio: f64,
    codebook_size: usize,
    codebook_dim: Option<usize>,
    use_conv_shortcut: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            target_bandwidths: vec![1.5, 3.0, 6.0, 12.0, 24.0],
            sampling_rate: 24_000,
            audio_channels: 1,
            normalize: false,
            chunk_length_s: None,
            overlap: None,
            hidden_size: 128,
            num_filters: 32,
            num_residual_layers: 1,
            upsampling_ratios: vec![8, 5, 4, 2],
            norm_type: NormType::WeightNorm,
            kernel_size: 7,
            last_kernel_size: 7,
            residual_kernel_size: 3,
            dilation_growth_rate: 2,
            use_causal_conv: true,
            pad_mode: "reflect",
            compress: 2,
            num_lstm_layers: 2,
            trim_right_ratio: 1.0,
            codebook_size: 1024,
            codebook_dim: None,
            use_conv_shortcut: true,
        }
    }
}

impl Config {
    // https://huggingface.co/facebook/musicgen-small/blob/495da4ad086b3416a27c6187f9239f9fd96f3962/config.json#L6
    pub fn musicgen_small() -> Self {
        Self {
            audio_channels: 1,
            chunk_length_s: None,
            codebook_dim: Some(128),
            codebook_size: 2048,
            compress: 2,
            dilation_growth_rate: 2,
            hidden_size: 128,
            kernel_size: 7,
            last_kernel_size: 7,
            norm_type: NormType::WeightNorm,
            normalize: false,
            num_filters: 64,
            num_lstm_layers: 2,
            num_residual_layers: 1,
            overlap: None,
            pad_mode: "reflect",
            residual_kernel_size: 3,
            sampling_rate: 32_000,
            target_bandwidths: vec![2.2],
            trim_right_ratio: 1.0,
            upsampling_ratios: vec![8, 5, 4, 4],
            use_causal_conv: false,
            use_conv_shortcut: false,
        }
    }

    fn codebook_dim(&self) -> usize {
        self.codebook_dim.unwrap_or(self.codebook_size)
    }

    fn frame_rate(&self) -> usize {
        let hop_length: usize = self.upsampling_ratios.iter().product();
        (self.sampling_rate + hop_length - 1) / hop_length
    }

    fn num_quantizers(&self) -> usize {
        let num = 1000f64
            * self
                .target_bandwidths
                .last()
                .expect("empty target_bandwidths");
        (num as usize) / (self.frame_rate() * 10)
    }
}

// https://github.com/huggingface/transformers/blob/abaca9f9432a84cfaa95531de4c72334f38a42f2/src/transformers/models/encodec/modeling_encodec.py#L340
#[derive(Debug)]
struct EncodecEuclideanCodebook {
    inited: Tensor,
    cluster_size: Tensor,
    embed: Tensor,
    embed_avg: Tensor,
}

impl EncodecEuclideanCodebook {
    fn load(p: &str, vb: &VarBuilder, cfg: &Config) -> Result<Self> {
        let inited = vb.get(1, &format!("{p}.inited"))?;
        let cluster_size = vb.get(cfg.codebook_size, &format!("{p}.cluster_size"))?;
        let e_shape = (cfg.codebook_size, cfg.codebook_dim());
        let embed = vb.get(e_shape, &format!("{p}.embed"))?;
        let embed_avg = vb.get(e_shape, &format!("{p}.embed_avg"))?;
        Ok(Self {
            inited,
            cluster_size,
            embed,
            embed_avg,
        })
    }
}

#[derive(Debug)]
struct EncodecVectorQuantization {
    codebook: EncodecEuclideanCodebook,
}

impl EncodecVectorQuantization {
    fn load(p: &str, vb: &VarBuilder, cfg: &Config) -> Result<Self> {
        let codebook = EncodecEuclideanCodebook::load(&format!("{p}.codebook"), vb, cfg)?;
        Ok(Self { codebook })
    }
}

#[derive(Debug)]
struct EncodecResidualVectorQuantizer {
    layers: Vec<EncodecVectorQuantization>,
}

impl EncodecResidualVectorQuantizer {
    fn load(p: &str, vb: &VarBuilder, cfg: &Config) -> Result<Self> {
        let p = format!("{p}.layers");
        let layers = (0..cfg.num_quantizers())
            .map(|i| EncodecVectorQuantization::load(&format!("{p}.{i}"), vb, cfg))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { layers })
    }
}

// https://github.com/huggingface/transformers/blob/abaca9f9432a84cfaa95531de4c72334f38a42f2/src/transformers/models/encodec/modeling_encodec.py#L226
#[derive(Debug)]
struct EncodecLSTM {
    layers: Vec<(Tensor, Tensor, Tensor, Tensor)>,
}

impl EncodecLSTM {
    fn load(dim: usize, p: &str, vb: &VarBuilder, cfg: &Config) -> Result<Self> {
        let p = format!("{p}.lstm");
        let mut layers = vec![];
        for i in 0..cfg.num_lstm_layers {
            let w_hh = vb.get((4 * dim, dim), &format!("{p}.weight_hh_l{i}"))?;
            let w_ih = vb.get((4 * dim, dim), &format!("{p}.weight_ih_l{i}"))?;
            let b_hh = vb.get(4 * dim, &format!("{p}.bias_hh_l{i}"))?;
            let b_ih = vb.get(4 * dim, &format!("{p}.bias_ih_l{i}"))?;
            layers.push((w_hh, w_ih, b_hh, b_ih))
        }
        Ok(Self { layers })
    }
}

#[derive(Debug)]
struct EncodecConvTranspose1d {
    weight_g: Tensor,
    weight_v: Tensor,
    bias: Tensor,
}

impl EncodecConvTranspose1d {
    fn load(
        in_c: usize,
        out_c: usize,
        k: usize,
        _stride: usize,
        p: &str,
        vb: &VarBuilder,
        _cfg: &Config,
    ) -> Result<Self> {
        let p = format!("{p}.conv");
        let weight_g = vb.get((in_c, 1, 1), &format!("{p}.weight_g"))?;
        let weight_v = vb.get((in_c, out_c, k), &format!("{p}.weight_v"))?;
        let bias = vb.get(out_c, &format!("{p}.bias"))?;
        Ok(Self {
            weight_g,
            weight_v,
            bias,
        })
    }
}

#[derive(Debug)]
struct EncodecConv1d {
    conv: Conv1D,
}

impl EncodecConv1d {
    fn load(
        in_c: usize,
        out_c: usize,
        kernel_size: usize,
        stride: usize,
        p: &str,
        vb: &VarBuilder,
        cfg: &Config,
    ) -> Result<Self> {
        let conv = match cfg.norm_type {
            NormType::WeightNorm => Conv1D::load_weight_norm(
                in_c,
                out_c,
                kernel_size,
                ConvConfig { padding: 0, stride },
                &format!("{p}.conv"),
                vb,
            )?,
            NormType::None => Conv1D::load(
                in_c,
                out_c,
                kernel_size,
                ConvConfig { padding: 0, stride },
                &format!("{p}.conv"),
                vb,
            )?,
        };
        Ok(Self { conv })
    }
}

#[derive(Debug)]
struct EncodecResnetBlock {
    block_conv1: EncodecConv1d,
    block_conv2: EncodecConv1d,
    shortcut: Option<EncodecConv1d>,
}

impl EncodecResnetBlock {
    fn load(
        dim: usize,
        dilations: &[usize],
        p: &str,
        vb: &VarBuilder,
        cfg: &Config,
    ) -> Result<Self> {
        let h = dim / cfg.compress;
        let mut layer = Layer::new(format!("{p}.block"));
        if dilations.len() != 2 {
            anyhow::bail!("expected dilations of size 2")
        }
        // TODO: Apply dilations!
        layer.inc();
        let block_conv1 = EncodecConv1d::load(
            dim,
            h,
            cfg.residual_kernel_size,
            1,
            &layer.next_name(),
            vb,
            cfg,
        )?;
        layer.inc();
        let block_conv2 = EncodecConv1d::load(h, dim, 1, 1, &layer.next_name(), vb, cfg)?;
        let shortcut = if cfg.use_conv_shortcut {
            let conv = EncodecConv1d::load(dim, dim, 1, 1, &format!("{p}.shortcut"), vb, cfg)?;
            Some(conv)
        } else {
            None
        };
        Ok(Self {
            block_conv1,
            block_conv2,
            shortcut,
        })
    }
}

#[derive(Debug)]
struct Layer {
    prefix: String,
    cnt: usize,
}

impl Layer {
    fn new(prefix: String) -> Self {
        Self { prefix, cnt: 0 }
    }

    fn inc(&mut self) {
        self.cnt += 1;
    }

    fn next_name(&mut self) -> String {
        let name = format!("{}.{}", self.prefix, self.cnt);
        self.cnt += 1;
        name
    }
}

#[derive(Debug)]
struct EncodecEncoder {
    init_conv: EncodecConv1d,
    sampling_layers: Vec<(Vec<EncodecResnetBlock>, EncodecConv1d)>,
    final_lstm: EncodecLSTM,
    final_conv: EncodecConv1d,
}

impl EncodecEncoder {
    fn load(p: &str, vb: &VarBuilder, cfg: &Config) -> Result<Self> {
        let mut layer = Layer::new(format!("{p}.layers"));
        let init_conv = EncodecConv1d::load(
            cfg.audio_channels,
            cfg.num_filters,
            cfg.kernel_size,
            1,
            &layer.next_name(),
            vb,
            cfg,
        )?;
        let mut sampling_layers = vec![];
        let mut scaling = 1;
        for &ratio in cfg.upsampling_ratios.iter().rev() {
            let current_scale = scaling * cfg.num_filters;
            let mut resnets = vec![];
            for j in 0..(cfg.num_residual_layers as u32) {
                let resnet = EncodecResnetBlock::load(
                    current_scale,
                    &[cfg.dilation_growth_rate.pow(j), 1],
                    &layer.next_name(),
                    vb,
                    cfg,
                )?;
                resnets.push(resnet)
            }
            layer.inc(); // ELU
            let conv1d = EncodecConv1d::load(
                current_scale,
                current_scale * 2,
                ratio * 2,
                ratio,
                &layer.next_name(),
                vb,
                cfg,
            )?;
            sampling_layers.push((resnets, conv1d));
            scaling *= 2;
        }
        let final_lstm = EncodecLSTM::load(cfg.num_filters * scaling, &layer.next_name(), vb, cfg)?;
        layer.inc(); // ELU
        let final_conv = EncodecConv1d::load(
            cfg.num_filters * scaling,
            cfg.hidden_size,
            cfg.last_kernel_size,
            1,
            &layer.next_name(),
            vb,
            cfg,
        )?;
        Ok(Self {
            init_conv,
            sampling_layers,
            final_conv,
            final_lstm,
        })
    }
}

#[derive(Debug)]
struct EncodecDecoder {
    init_conv: EncodecConv1d,
    init_lstm: EncodecLSTM,
    sampling_layers: Vec<(EncodecConvTranspose1d, Vec<EncodecResnetBlock>)>,
    final_conv: EncodecConv1d,
}

impl EncodecDecoder {
    fn load(p: &str, vb: &VarBuilder, cfg: &Config) -> Result<Self> {
        let mut layer = Layer::new(format!("{p}.layers"));
        let mut scaling = usize::pow(2, cfg.upsampling_ratios.len() as u32);
        let init_conv = EncodecConv1d::load(
            cfg.hidden_size,
            cfg.num_filters * scaling,
            cfg.last_kernel_size,
            1,
            &layer.next_name(),
            vb,
            cfg,
        )?;
        let init_lstm = EncodecLSTM::load(cfg.num_filters * scaling, &layer.next_name(), vb, cfg)?;
        let mut sampling_layers = vec![];
        for &ratio in cfg.upsampling_ratios.iter() {
            let current_scale = scaling * cfg.num_filters;
            layer.inc(); // ELU
            let conv1d = EncodecConvTranspose1d::load(
                current_scale,
                current_scale / 2,
                ratio * 2,
                ratio,
                &layer.next_name(),
                vb,
                cfg,
            )?;
            let mut resnets = vec![];
            for j in 0..(cfg.num_residual_layers as u32) {
                let resnet = EncodecResnetBlock::load(
                    current_scale / 2,
                    &[cfg.dilation_growth_rate.pow(j), 1],
                    &layer.next_name(),
                    vb,
                    cfg,
                )?;
                resnets.push(resnet)
            }
            sampling_layers.push((conv1d, resnets));
            scaling /= 2;
        }
        layer.inc(); // ELU
        let final_conv = EncodecConv1d::load(
            cfg.num_filters,
            cfg.audio_channels,
            cfg.last_kernel_size,
            1,
            &layer.next_name(),
            vb,
            cfg,
        )?;
        Ok(Self {
            init_conv,
            init_lstm,
            sampling_layers,
            final_conv,
        })
    }
}

#[derive(Debug)]
pub struct EncodecModel {
    encoder: EncodecEncoder,
    decoder: EncodecDecoder,
    quantizer: EncodecResidualVectorQuantizer,
}

impl EncodecModel {
    pub fn load(p: &str, vb: &VarBuilder, cfg: &Config) -> Result<Self> {
        let encoder = EncodecEncoder::load(&format!("{p}.encoder"), vb, cfg)?;
        let decoder = EncodecDecoder::load(&format!("{p}.decoder"), vb, cfg)?;
        let quantizer = EncodecResidualVectorQuantizer::load(&format!("{p}.quantizer"), vb, cfg)?;
        Ok(Self {
            encoder,
            decoder,
            quantizer,
        })
    }
}