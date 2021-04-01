extern crate clap;
extern crate cubeglobe;
extern crate elefren;
#[macro_use]
extern crate serde_derive;
extern crate anyhow;
extern crate image;
extern crate serde;
extern crate toml;
#[macro_use]
extern crate thiserror;
extern crate chrono;
extern crate rand;
extern crate oxipng;

use std::fs::{create_dir_all, read, read_to_string, File};
use std::io::{BufReader, Write};
use std::io::{Cursor, Read, Seek};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration as StdDuration;

use chrono::prelude::*;
use chrono::Duration as ChrDuration;
use clap::{App, Arg};
use elefren::Data as MastoData;
use elefren::{Mastodon, MastodonClient, MediaBuilder, StatusBuilder};
use anyhow::Error;
use image::{ImageError, ImageOutputFormat};
use rand::{thread_rng, Rng};

use cubeglobe::map::generator::{Generator, TerGenTwo};
use cubeglobe::renderer::{RWops, Renderer, RendererError, Surface};

const STATE_PATH: &str = "state";
const IMAGES_DIR: &str = "images";
const IMAGE_TITLE: &str = "A procedurally generated landscape composed of cuboid blocks, rendered in isometric perspective.";
const POST_BODY: &str = "⛰️";
// 30 seconds, 1 minute, 5 minutes, 15 minutes
const DELAYS: &[u64] = &[30, 60, 300, 900];

#[derive(Deserialize)]
struct ConfigFile {
    bot: BotConfig,
    credentials: MastoData,
}

#[derive(Deserialize)]
struct BotConfig {
    #[serde(default = "default_sleep_time")]
    sleep_time: i64,

    #[serde(default = "default_jitter")]
    jitter: i64,

    map_size: usize,

    min_frequency: Option<f64>,
    max_frequency: Option<f64>,

    layer_height: Option<usize>,
    min_soil_cutoff: Option<usize>,
    max_water_level: Option<usize>,
}

fn default_sleep_time() -> i64 {
    3600
}
fn default_jitter() -> i64 {
    300
}

/// Current state of the bot
///
/// The bot uses this struct, backed by a toml file on disk, to keep track of its state. The bot
/// first waits for the next posting time, then generates the image, then posts the image, then
/// waits again. We keep track of the state so that if remote problems cause posting to fail, we
/// attempt to retry the last image instead of generating a new one.
#[derive(Deserialize, Serialize)]
struct State {
    last_post: Option<DateTime<Utc>>,
    id: u32,
    phase: Phase,
}

#[derive(Deserialize, Serialize)]
enum Phase {
    Awaiting,
    Generated,
}

impl Default for State {
    fn default() -> State {
        State {
            last_post: None,
            id: 1,
            phase: Phase::Awaiting,
        }
    }
}

impl State {
    /// Read state from file or otherwise get a new one with defaults
    fn get_state() -> State {
        read_to_string(STATE_PATH)
            .ok()
            .and_then(|ref s| toml::from_str::<State>(s).ok())
            .unwrap_or_default()
    }

    /// Save current state to file
    fn persist(&self) -> Result<(), Error> {
        let serialized = toml::to_string(self)?;
        let mut statefile = File::create(STATE_PATH)?;

        statefile.write_all(serialized.as_bytes())?;

        Ok(())
    }

    /// Get the full filepath for where to save the current image file
    fn get_filename(&self) -> Result<Box<Path>, Error> {
        let mut pathbuf = PathBuf::new();
        pathbuf.push(IMAGES_DIR);
        create_dir_all(&pathbuf)?;

        pathbuf.push(format!("{}", self.id));
        pathbuf.set_extension("png");
        Ok(pathbuf.into_boxed_path())
    }

    fn get_saved_image(&self) -> Result<Vec<u8>, Error> {
        if let Phase::Awaiting = self.phase {
            return Err(BadStateError(
                "Asked to load image but currently in Awaiting state".to_string(),
            ).into());
        }

        Ok(read(self.get_filename()?)?)
    }

    /// Update state to indicate posting was successful
    fn posted(self) -> State {
        State {
            last_post: Some(Utc::now()),
            id: self.id + 1,
            phase: Phase::Awaiting,
        }
    }

    /// Update state to indicate image was generated but not yet posted
    fn generated(self) -> State {
        State {
            phase: Phase::Generated,
            ..self
        }
    }

    /// Post new status, with `image`
    fn post_status<I>(&self, masto: &Mastodon, image: I) -> Result<(), PostingError>
    where
        I: Read + Send + 'static,
    {
        let attachment = masto.new_media(MediaBuilder {
            description: Some(IMAGE_TITLE.to_string()),
            mimetype: Some("image/png".to_string()),
            filename: Some(format!("{}.png", self.id)),
            ..MediaBuilder::from_reader(image)
        })?;
        let status = masto.new_status(StatusBuilder {
            media_ids: Some(vec![attachment.id.parse::<u64>()?]),
            visibility: Some(elefren::status_builder::Visibility::Public),
            ..StatusBuilder::new(POST_BODY.to_string())
        })?;

        eprintln!("New status posted at: {}", status.uri);

        Ok(())
    }
}

/// Generate a new map and render it to a `Surface`
fn generate_image<'a>(
    config: &BotConfig,
    renderer: &Renderer,
) -> Result<Surface<'a>, RendererError> {
    let mut generator = TerGenTwo::new().set_len(config.map_size);
    let mut rng = thread_rng();

    if let Some(min) = config.min_frequency {
        if let Some(max) = config.max_frequency {
            generator = generator.set_frequency(rng.gen_range(min, max));
        }
    }

    if let Some(height) = config.layer_height {
        generator = generator.set_layer_height(height);
    }

    if let Some(cutoff) = config.min_soil_cutoff {
        generator = generator.set_min_soil_cutoff(cutoff);
    }

    if let Some(level) = config.max_water_level {
        generator = generator.set_max_water_level(level);
    }

    let map = generator.generate();

    renderer.render_map(&map)
}

#[derive(Error, Debug)]
pub enum ImageConvertError {
    #[error("SDL error: {0}")]
    SdlError(String),
    #[error("Error loading image: {0}")]
    ImageError(#[from] ImageError),
}

#[derive(Error, Debug)]
#[error("function called while in incorrect state")]
pub struct BadStateError(String);

#[derive(Error, Debug)]
pub enum PostingError {
    #[error("Elefren returned an arror: {0}")]
    ElefrenError(#[from] elefren::Error),
}

/// Take a surface and write to to writer `out`, as PNG
fn write_surface_as_png<W: Write>(surf: &Surface, mut out: W) -> Result<(), Error> {
    let (width, height) = surf.size();

    // each line is padded to multiple of four
    let line_mem_size = (width * 3) + ((width * 3) % 4);

    // header should be 54. It can theoretically be longer, but hopefully not or things will go
    // terribly for us
    let mem_size = line_mem_size * height + 54;

    // Ugliness alert: The only way to write to memory from a Surface (instead of writing to a file)
    // is through RWOps. We have to allocate some memory and give it a slice to write to.
    let mut surf_bytes: Vec<u8> = vec![0; mem_size as usize];
    // from_bytes_mut can only fail if surf_bytes len is zero
    let mut rwops =
        RWops::from_bytes_mut(&mut surf_bytes).expect("zero size buffer allocated for bmp");
    surf.save_bmp_rw(&mut rwops)
        .map_err(ImageConvertError::SdlError)?;

    rwops.seek(std::io::SeekFrom::Start(0))?;

    image::load(BufReader::new(rwops), image::ImageFormat::BMP)
        .map_err(ImageConvertError::ImageError)?
        .write_to(&mut out, ImageOutputFormat::PNG)
        .map_err(ImageConvertError::ImageError)?;
    Ok(())
}

fn get_backoff(attempt: usize) -> u64 {
    // Note: attempt is 1-indexed (first attempt is number 1)
    if attempt > DELAYS.len() {
        *DELAYS.last().unwrap()
    } else {
        DELAYS[attempt - 1]
    }
}

fn main() {
    let matches = App::new("cubeglobe-bot")
        .version("0.1.1")
        .arg(
            Arg::with_name("config")
                .short("c")
                .long("config")
                .value_name("PATH")
                .help("path to the main config file"),
        ).arg(
            Arg::with_name("tilesconfig")
                .short("t")
                .long("tiles")
                .value_name("PATH")
                .help("path to the tiles configuration file"),
        ).arg(
            Arg::with_name("immediate")
                .long("immediate")
                .help("immediately generate and post an image, and then exit"),
        ).get_matches();

    let config_path = matches.value_of("config").unwrap_or("config.toml");
    let tiles_config_path = matches.value_of("tilesconfig").unwrap_or("tiles.conf");

    let config: ConfigFile =
        toml::from_str(&read_to_string(config_path).expect("Unable to read bot config"))
            .expect("Problem reading bot config");

    let fedi = Mastodon::from(config.credentials);

    let renderer = Renderer::from_config_str(
        &read_to_string(tiles_config_path).expect("Unable to read tiles config"),
    ).expect("Problem initializing renderer");

    let mut state = State::get_state();

    // Immediate mode posts immediately and exits. We do not try to retry at all here.
    if matches.is_present("immediate") {
        eprintln!("Immediate post requested, generating...");
        let surf = generate_image(&config.bot, &renderer).expect("Problem generating image");
        let filename = state
            .get_filename()
            .expect("Failed to initalize the images subdirectory");
        let mut image_data: Vec<u8> = Vec::new();
        write_surface_as_png(&surf, image_data.by_ref()).expect("Unable to generate png");
        
        image_data = match oxipng::optimize_from_memory(&image_data, &oxipng::Options::from_preset(4)) {
            Ok(new_image) => new_image,
            Err(e) => {
                eprintln!("Failed to optimize PNG, falling back to unoptimized: {}", e);
                image_data 
            }
        };

        {
            let mut outfile = File::create(&filename).expect("Unable to create image file");
            outfile
                .write_all(&image_data)
                .expect("Unable to write to file");
        }
        eprintln!(
            "Generated image file: {}",
            &filename
                .to_str()
                .expect("Something went terribly wrong figuring out the image filename")
        );

        state = state.generated();
        state.persist().expect("Unable to persist state");
        state
            .post_status(&fedi, Cursor::new(image_data))
            .expect("Failed to post status");

        state.posted().persist().expect("Unable to persist state");
    } else {
        let mut current_image: Option<Vec<u8>> = None;
        let mut attempt: usize = 0;

        loop {
            if let Phase::Awaiting = state.phase {
                if let Some(last_post) = state.last_post {
                    let mut rng = thread_rng();
                    let total_to_wait = ChrDuration::seconds(
                        config.bot.sleep_time
                            + rng.gen_range(0 - config.bot.jitter, config.bot.jitter),
                    );

                    let scheduled = last_post + total_to_wait;
                    let actual_to_wait = scheduled - Utc::now();

                    if actual_to_wait < ChrDuration::zero() {
                        eprintln!(
                            "Post was due at {}, it is now later, starting new post...",
                            scheduled
                        );
                    } else {
                        eprintln!("Sleeping until {}...", scheduled);
                        sleep(actual_to_wait.to_std().expect("Time duration too large"));
                        eprintln!("Done sleeping, starting new post...");
                    }
                } else {
                    eprintln!("State shows no previous post, starting first one...");
                }

                let surf =
                    generate_image(&config.bot, &renderer).expect("Problem generating image");
                let filename = state
                    .get_filename()
                    .expect("Failed to initalize the images subdirectory");
                let mut new_image = Vec::new();
                write_surface_as_png(&surf, new_image.by_ref()).expect("Unable to generate png");

                new_image = match oxipng::optimize_from_memory(&new_image, &oxipng::Options::from_preset(4)) {
                    Ok(optimized) => optimized,
                    Err(e) => {
                        eprintln!("Failed to optimize PNG, falling back to unoptimized: {}", e);
                        new_image
                    }
                };

                {
                    let mut outfile = File::create(&filename).expect("Unable to create image file");
                    outfile
                        .write_all(&new_image)
                        .expect("Unable to write to file");
                }
                eprintln!(
                    "Generated image file: {}",
                    &filename
                        .to_str()
                        .expect("Something went terribly wrong figuring out the image filename")
                );

                current_image = Some(new_image);
                state = state.generated();
                state.persist().expect("Unable to persist state");
            }

            if let Phase::Generated = state.phase {
                let image_data = current_image.unwrap_or_else(|| {
                    state
                        .get_saved_image()
                        .expect("Wanted to retry uploading image but was unable to open its file")
                });

                attempt += 1;
                // TODO: Figure out a way to use a reader here that can share memory here OR see if
                // giving elefren a variant that uses reqwest's bytes() could help us avoid a clone
                // here somehow 
                let result = state.post_status(&fedi, Cursor::new(image_data.clone())); // 😬

                match result {
                    Ok(_) => {
                        attempt = 0;
                        state = state.posted();
                        state.persist().expect("Unable to persist state");
                        current_image = None;
                    }
                    Err(e) => {
                        eprintln!("Failed to post: {}", e);
                        let backoff = get_backoff(attempt);
                        eprintln!("Retrying after {} seconds", backoff);
                        sleep(StdDuration::from_secs(backoff));
                        current_image = Some(image_data);
                    }
                }
            }
        }
    }
}
