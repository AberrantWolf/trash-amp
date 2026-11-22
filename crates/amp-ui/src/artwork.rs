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
type RowData = (BrowserRow, Option<PathBuf>);
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
    if changed.is_empty() {
        return false;
    }
    for model in models {
        let rows: Vec<_> = model
            .requested_paths
            .borrow()
            .iter()
            .filter_map(|(row, path)| changed.contains(path).then_some(*row))
            .collect();
        for row in rows {
            model.notify.row_changed(row);
        }
    }
    true
}
struct ArtworkModel {
    count: std::cell::Cell<usize>,
    data: Box<dyn Fn(usize) -> Option<RowData>>,
    requested_paths: RefCell<HashMap<usize, PathBuf>>,
    notify: ModelNotify,
}
impl Model for ArtworkModel {
    type Data = BrowserRow;
    fn row_count(&self) -> usize {
        self.count.get()
    }
    fn row_data(&self, row: usize) -> Option<BrowserRow> {
        let (mut value, path) = (self.data)(row)?;
        value.artwork = if let Some(path) = path {
            let artwork = load(&path).unwrap_or_default();
            self.requested_paths.borrow_mut().insert(row, path);
            artwork
        } else {
            slint::Image::default()
        };
        Some(value)
    }
    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}
pub fn model(rows: Vec<BrowserRow>, paths: Vec<Option<PathBuf>>) -> ModelRc<BrowserRow> {
    lazy_model(rows.len(), move |row| {
        Some((rows.get(row)?.clone(), paths.get(row).cloned().flatten()))
    })
}

pub fn lazy_model(
    count: usize,
    data: impl Fn(usize) -> Option<RowData> + 'static,
) -> ModelRc<BrowserRow> {
    lazy_model_rc(count, data).into()
}

fn lazy_model_rc(
    count: usize,
    data: impl Fn(usize) -> Option<RowData> + 'static,
) -> Rc<ArtworkModel> {
    let model = Rc::new(ArtworkModel {
        count: count.into(),
        data: Box::new(data),
        requested_paths: RefCell::default(),
        notify: ModelNotify::default(),
    });
    CACHE.with_borrow_mut(|cache| cache.models.push(Rc::downgrade(&model)));
    model
}

/// Rows that can be edited in place, so a removal or move only touches the
/// affected rows and the rest of the view keeps its state and scroll position.
pub struct Rows {
    model: Rc<ArtworkModel>,
    rows: Rc<RefCell<Vec<RowData>>>,
}

impl Rows {
    pub fn new(rows: Vec<BrowserRow>, paths: Vec<Option<PathBuf>>) -> Self {
        let mut paths = paths.into_iter();
        let rows = Rc::new(RefCell::new(
            rows.into_iter()
                .map(|row| (row, paths.next().flatten()))
                .collect::<Vec<_>>(),
        ));
        let data = rows.clone();
        let count = rows.borrow().len();
        Self {
            model: lazy_model_rc(count, move |row| data.borrow().get(row).cloned()),
            rows,
        }
    }

    pub fn model(&self) -> ModelRc<BrowserRow> {
        ModelRc::from(self.model.clone())
    }

    pub fn remove(&self, index: usize) {
        let mut rows = self.rows.borrow_mut();
        if index >= rows.len() {
            return;
        }
        rows.remove(index);
        self.model.count.set(rows.len());
        drop(rows);
        // Requested rows are keyed by position; rows re-register when read.
        self.model.requested_paths.borrow_mut().clear();
        self.model.notify.row_removed(index, 1);
    }

    pub fn move_row(&self, from: usize, to: usize) {
        let mut rows = self.rows.borrow_mut();
        if from >= rows.len() || to >= rows.len() || from == to {
            return;
        }
        let row = rows.remove(from);
        rows.insert(to, row);
        drop(rows);
        self.model.requested_paths.borrow_mut().clear();
        self.model.notify.row_removed(from, 1);
        self.model.notify.row_added(to, 1);
    }
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
    fn editable_rows_remove_and_move_in_place() {
        let row = |title: &str| BrowserRow {
            title: title.into(),
            ..Default::default()
        };
        let rows = Rows::new(vec![row("a"), row("b"), row("c")], vec![]);
        let model = rows.model();
        let titles = || {
            (0..model.row_count())
                .map(|i| model.row_data(i).unwrap().title.to_string())
                .collect::<Vec<_>>()
        };
        rows.remove(1);
        assert_eq!(titles(), ["a", "c"]);
        rows.move_row(0, 1);
        assert_eq!(titles(), ["c", "a"]);
        rows.remove(5);
        assert_eq!(titles(), ["c", "a"]);
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
