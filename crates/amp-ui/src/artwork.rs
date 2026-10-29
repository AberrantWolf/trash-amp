//! Lazy, shared thumbnails. Only rows requested by the view schedule decoding.
use crate::BrowserRow;
use slint::{Model, ModelNotify, ModelRc, ModelTracker};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    rc::{Rc, Weak},
    sync::mpsc,
};

const CACHE_IMAGES: usize = 128;
const THUMBNAIL_SIZE: u32 = 128;
type Pixels = Option<(Vec<u8>, u32, u32)>;
struct Cache {
    images: HashMap<PathBuf, Option<slint::Image>>,
    recent: VecDeque<PathBuf>,
    pending: HashSet<PathBuf>,
    requests: mpsc::SyncSender<PathBuf>,
    replies: mpsc::Receiver<(PathBuf, Pixels)>,
    models: Vec<Weak<ArtworkModel>>,
}
impl Default for Cache {
    fn default() -> Self {
        let (requests, jobs) = mpsc::sync_channel::<PathBuf>(32);
        let (tx, replies) = mpsc::sync_channel(32);
        std::thread::Builder::new()
            .name("artwork-thumbnails".into())
            .spawn(move || {
                while let Ok(path) = jobs.recv() {
                    let pixels = decode(&path);
                    if tx.send((path, pixels)).is_err() {
                        break;
                    }
                }
            })
            .expect("start artwork worker");
        Self {
            images: HashMap::new(),
            recent: VecDeque::new(),
            pending: HashSet::new(),
            requests,
            replies,
            models: vec![],
        }
    }
}
thread_local! { static CACHE: RefCell<Cache> = RefCell::new(Cache::default()); }

fn decode(path: &Path) -> Pixels {
    let mut reader = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader.decode().ok()?;
    let pixels = if decoded.width() > THUMBNAIL_SIZE || decoded.height() > THUMBNAIL_SIZE {
        decoded.thumbnail(THUMBNAIL_SIZE, THUMBNAIL_SIZE)
    } else {
        decoded
    }
    .into_rgba8();
    let (width, height) = pixels.dimensions();
    Some((pixels.into_raw(), width, height))
}
/// Never reads the filesystem on the presentation thread.
pub fn load(path: &Path) -> Option<slint::Image> {
    CACHE.with_borrow_mut(|cache| {
        if let Some(image) = cache.images.get(path).cloned() {
            cache.recent.retain(|key| key != path);
            cache.recent.push_back(path.to_owned());
            return image;
        }
        if !cache.pending.contains(path) && cache.requests.try_send(path.to_owned()).is_ok() {
            cache.pending.insert(path.to_owned());
        }
        None
    })
}
/// Called by the presentation timer; row notifications only touch current models.
pub fn poll() -> bool {
    let (changed, models) = CACHE.with_borrow_mut(|cache| {
        let mut changed = HashSet::new();
        while let Ok((path, pixels)) = cache.replies.try_recv() {
            cache.pending.remove(&path);
            let image = pixels.map(|(bytes, width, height)| {
                slint::Image::from_rgba8(
                    slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                        &bytes, width, height,
                    ),
                )
            });
            cache.recent.retain(|key| key != &path);
            cache.recent.push_back(path.clone());
            cache.images.insert(path.clone(), image);
            changed.insert(path);
            while cache.images.len() > CACHE_IMAGES {
                if let Some(old) = cache.recent.pop_front() {
                    cache.images.remove(&old);
                }
            }
        }
        cache.models.retain(|model| model.strong_count() > 0);
        (
            changed,
            cache
                .models
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>(),
        )
    });
    for model in models {
        for (index, path) in model.paths.iter().enumerate() {
            if path.as_ref().is_some_and(|path| changed.contains(path)) {
                model.notify.row_changed(index);
            }
        }
    }
    !changed.is_empty()
}
struct ArtworkModel {
    rows: Vec<BrowserRow>,
    paths: Vec<Option<PathBuf>>,
    notify: ModelNotify,
}
impl Model for ArtworkModel {
    type Data = BrowserRow;
    fn row_count(&self) -> usize {
        self.rows.len()
    }
    fn row_data(&self, row: usize) -> Option<BrowserRow> {
        let mut value = self.rows.get(row)?.clone();
        value.artwork = self
            .paths
            .get(row)
            .and_then(Option::as_deref)
            .and_then(load)
            .unwrap_or_default();
        Some(value)
    }
    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}
pub fn model(rows: Vec<BrowserRow>, paths: Vec<Option<PathBuf>>) -> ModelRc<BrowserRow> {
    let model = Rc::new(ArtworkModel {
        rows,
        paths,
        notify: ModelNotify::default(),
    });
    CACHE.with_borrow_mut(|cache| cache.models.push(Rc::downgrade(&model)));
    ModelRc::from(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_thumbnails_and_accepts_extensionless_images() {
        let path = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let image = image::RgbImage::from_pixel(1024, 512, image::Rgb([20, 40, 60]));
        image
            .save_with_format(&path, image::ImageFormat::Png)
            .unwrap();
        let (_, width, height) = decode(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!((width, height), (128, 64));
    }
    #[test]
    fn invalid_artwork_falls_back() {
        assert!(decode(Path::new("/missing/artwork")).is_none());
    }
    #[test]
    fn models_schedule_only_requested_rows_and_share_cached_images() {
        let path = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        image::RgbImage::new(2, 3)
            .save_with_format(&path, image::ImageFormat::Png)
            .unwrap();
        let model = model(
            vec![BrowserRow::default(); 200],
            vec![Some(path.clone()); 200],
        );
        CACHE.with_borrow(|cache| assert!(!cache.pending.contains(&path)));
        assert_eq!(model.row_data(0).unwrap().artwork.size().width, 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while model.row_data(0).unwrap().artwork.size().width == 0 {
            assert!(std::time::Instant::now() < deadline);
            poll();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(model.row_data(100).unwrap().artwork.size().width, 2);
        CACHE.with_borrow(|cache| assert!(cache.images.len() <= CACHE_IMAGES));
        std::fs::remove_file(path).unwrap();
    }
}
