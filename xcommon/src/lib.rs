use anyhow::Result;
use byteorder::{LittleEndian, ReadBytesExt};
use image::imageops::FilterType;
use image::io::Reader as ImageReader;
use image::{DynamicImage, GenericImageView, ImageOutputFormat, RgbaImage};
use rsa::pkcs8::FromPrivateKey;
use rsa::{Hash, PaddingScheme, RsaPrivateKey, RsaPublicKey};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::Path;
use zip::write::FileOptions;
use zip::{CompressionMethod, ZipWriter};

pub use rasn_pkix::Certificate;
pub use zip::read::ZipFile;

pub struct Scaler {
    img: DynamicImage,
}

impl Scaler {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let img = ImageReader::open(path)?.decode()?;
        let (width, height) = img.dimensions();
        if width != height {
            anyhow::bail!("expected width == height");
        }
        if width < 512 {
            anyhow::bail!("expected icon of at least 512x512 px");
        }
        Ok(Self { img })
    }

    pub fn optimize(&mut self) {
        let mut is_grayscale = true;
        let mut is_opaque = true;
        let (width, height) = self.img.dimensions();
        for x in 0..width {
            for y in 0..height {
                let pixel = self.img.get_pixel(x, y);
                if pixel[0] != pixel[1] || pixel[1] != pixel[2] {
                    is_grayscale = false;
                }
                if pixel[3] != 255 {
                    is_opaque = false;
                }
                if !is_grayscale && !is_opaque {
                    break;
                }
            }
        }
        match (is_grayscale, is_opaque) {
            (true, true) => self.img = DynamicImage::ImageLuma8(self.img.to_luma8()),
            (true, false) => self.img = DynamicImage::ImageLumaA8(self.img.to_luma_alpha8()),
            (false, true) => self.img = DynamicImage::ImageRgb8(self.img.to_rgb8()),
            (false, false) => {}
        }
    }

    pub fn write<W: Write + Seek>(&self, w: &mut W, opts: ScalerOpts) -> Result<()> {
        let resized = self
            .img
            .resize(opts.scaled_size, opts.scaled_size, FilterType::Nearest);
        if opts.scaled_size == opts.target_width && opts.scaled_size == opts.target_height {
            resized.write_to(w, ImageOutputFormat::Png)?;
        } else {
            let x = (opts.target_width - opts.scaled_size) / 2;
            let y = (opts.target_height - opts.scaled_size) / 2;
            let mut padded = RgbaImage::new(opts.target_width, opts.target_height);
            image::imageops::overlay(&mut padded, &resized, x as i64, y as i64);
            padded.write_to(w, ImageOutputFormat::Png)?;
        }
        Ok(())
    }

    pub fn to_vec(&self, opts: ScalerOpts) -> Vec<u8> {
        let mut buf = vec![];
        let mut cursor = Cursor::new(&mut buf);
        self.write(&mut cursor, opts).unwrap();
        buf
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScalerOptsBuilder {
    width: u32,
    height: u32,
    scale: f32,
    padding: f32,
}

impl ScalerOptsBuilder {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            scale: 1.0,
            padding: 0.0,
        }
    }

    pub fn scale(mut self, scale: f32) -> Self {
        self.scale = scale;
        self
    }

    pub fn padding(mut self, percent: f32) -> Self {
        self.padding = percent;
        self
    }

    pub fn build(self) -> ScalerOpts {
        let target_width = (self.width as f32 * self.scale) as u32;
        let target_height = (self.height as f32 * self.scale) as u32;
        let unpadded_size = std::cmp::min(target_width, target_height);
        let scaled_size = (unpadded_size as f32 - (unpadded_size as f32 * self.padding)) as u32;
        ScalerOpts {
            target_width,
            target_height,
            scaled_size,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScalerOpts {
    target_width: u32,
    target_height: u32,
    scaled_size: u32,
}

impl ScalerOpts {
    pub fn new(size: u32) -> Self {
        Self {
            target_width: size,
            target_height: size,
            scaled_size: size,
        }
    }
}

#[derive(Clone)]
pub struct Signer {
    key: RsaPrivateKey,
    pubkey: RsaPublicKey,
    cert: Certificate,
}

impl Signer {
    /// Creates a new signer using a private key and a certificate.
    ///
    /// A new self signed certificate can be generated using openssl:
    /// ```sh
    /// openssl req -newkey rsa:2048 -new -nodes -x509 -days 3650 -keyout key.pem -out cert.pem
    /// ```
    pub fn new(private_key: &str, certificate: &str) -> Result<Self> {
        let key = RsaPrivateKey::from_pkcs8_pem(private_key)?;
        let pubkey = RsaPublicKey::from(&key);
        let pem = pem::parse(certificate)?;
        anyhow::ensure!(pem.tag == "CERTIFICATE");
        let cert = rasn::der::decode::<Certificate>(&pem.contents)
            .map_err(|err| anyhow::anyhow!("{}", err))?;
        Ok(Self { key, pubkey, cert })
    }

    pub fn sign(&self, bytes: &[u8]) -> Vec<u8> {
        let digest = Sha256::digest(bytes);
        let padding = PaddingScheme::new_pkcs1v15_sign(Some(Hash::SHA2_256));
        self.key.sign(padding, &digest).unwrap()
    }

    pub fn pubkey(&self) -> &RsaPublicKey {
        &self.pubkey
    }

    pub fn cert(&self) -> &Certificate {
        &self.cert
    }
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("Signer")
            .field("pubkey", &self.pubkey)
            .field("cert", &self.cert)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ZipFileOptions {
    Unaligned,
    Aligned(u16),
    Compressed,
}

impl ZipFileOptions {
    pub fn alignment(self) -> u16 {
        match self {
            Self::Aligned(align) => align,
            _ => 1,
        }
    }

    pub fn compression_method(&self) -> CompressionMethod {
        match self {
            Self::Compressed => CompressionMethod::Deflated,
            _ => CompressionMethod::Stored,
        }
    }
}

pub struct ZipInfo {
    pub cde_start: u64,
    pub cd_start: u64,
}

impl ZipInfo {
    pub fn new<R: Read + Seek>(r: &mut R) -> Result<Self> {
        let cde_start = find_cde_start_pos(r)?;
        r.seek(SeekFrom::Start(cde_start + 16))?;
        let cd_start = r.read_u32::<LittleEndian>()? as u64;
        Ok(Self {
            cde_start,
            cd_start,
        })
    }
}

// adapted from zip-rs
fn find_cde_start_pos<R: Read + Seek>(reader: &mut R) -> Result<u64> {
    const CENTRAL_DIRECTORY_END_SIGNATURE: u32 = 0x06054b50;
    const HEADER_SIZE: u64 = 22;
    let file_length = reader.seek(SeekFrom::End(0))?;
    let search_upper_bound = file_length.saturating_sub(HEADER_SIZE + ::std::u16::MAX as u64);
    if file_length < HEADER_SIZE {
        anyhow::bail!("Invalid zip header");
    }
    let mut pos = file_length - HEADER_SIZE;
    while pos >= search_upper_bound {
        reader.seek(SeekFrom::Start(pos as u64))?;
        if reader.read_u32::<LittleEndian>()? == CENTRAL_DIRECTORY_END_SIGNATURE {
            return Ok(pos);
        }
        pos = match pos.checked_sub(1) {
            Some(p) => p,
            None => break,
        };
    }
    anyhow::bail!("Could not find central directory end");
}

pub struct Zip {
    zip: ZipWriter<File>,
}

impl Zip {
    pub fn new(path: &Path) -> Result<Self> {
        Ok(Self {
            zip: ZipWriter::new(File::create(path)?),
        })
    }

    pub fn append(path: &Path) -> Result<Self> {
        let f = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Self {
            zip: ZipWriter::new_append(f)?,
        })
    }

    pub fn add_file(&mut self, source: &Path, dest: &Path, opts: ZipFileOptions) -> Result<()> {
        let mut f = File::open(source)?;
        self.start_file(dest, opts)?;
        std::io::copy(&mut f, &mut self.zip)?;
        Ok(())
    }

    pub fn add_directory(&mut self, source: &Path, dest: &Path) -> Result<()> {
        add_recursive(self, source, dest)?;
        Ok(())
    }

    pub fn add_zip_file(&mut self, f: ZipFile) -> Result<()> {
        self.zip.raw_copy_file(f)?;
        Ok(())
    }

    pub fn create_file(
        &mut self,
        dest: &Path,
        opts: ZipFileOptions,
        contents: &[u8],
    ) -> Result<()> {
        self.start_file(dest, opts)?;
        self.zip.write_all(contents)?;
        Ok(())
    }

    pub fn start_file(&mut self, dest: &Path, opts: ZipFileOptions) -> Result<()> {
        let name = dest
            .iter()
            .map(|seg| seg.to_str().unwrap())
            .collect::<Vec<_>>()
            .join("/");
        let zopts = FileOptions::default().compression_method(opts.compression_method());
        self.zip.start_file_aligned(name, zopts, opts.alignment())?;
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        self.zip.finish()?;
        Ok(())
    }
}

fn add_recursive(zip: &mut Zip, source: &Path, dest: &Path) -> Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let source = source.join(&file_name);
        let dest = dest.join(&file_name);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            add_recursive(zip, &source, &dest)?;
        } else if file_type.is_file() {
            zip.add_file(&source, &dest, ZipFileOptions::Compressed)?;
        }
    }
    Ok(())
}

impl Write for Zip {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.zip.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.zip.flush()
    }
}

pub fn copy_dir_all(source: &Path, dest: &Path) -> Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let source = source.join(&file_name);
        let dest = dest.join(&file_name);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            std::fs::create_dir_all(&dest)?;
            copy_dir_all(&source, &dest)?;
        } else if file_type.is_file() {
            std::fs::copy(&source, &dest)?;
        } else if file_type.is_symlink() {
            let target = std::fs::read_link(&source)?;
            std::os::unix::fs::symlink(target, dest)?;
        }
    }
    Ok(())
}

pub fn stamp_file(file: &Path, stamp: &Path) -> Result<bool> {
    let stamp_exists = stamp.exists();
    if !stamp_exists {
        if let Some(parent) = stamp.parent() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let stamp_time = File::create(stamp)?.metadata()?.modified()?;
    let file_time = File::open(file)?.metadata()?.modified()?;
    Ok(!stamp_exists || file_time > stamp_time)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = include_str!("../../assets/key.pem");
    const CERT: &str = include_str!("../../assets/cert.pem");

    #[test]
    fn create_signer() {
        Signer::new(KEY, CERT).unwrap();
    }
}