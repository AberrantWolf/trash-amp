//! Device music metadata and opaque asset resolution through MediaPlayer.
use super::*;
use objc2::{Encode, Encoding};
use objc2_foundation::NSNumber;
unsafe fn query(id: Option<&str>) -> Result<Retained<NSArray<AnyObject>>> {
    let query: Retained<AnyObject> = unsafe { msg_send![class(c"MPMediaQuery"), songsQuery] };
    if let Some(id) = id {
        let value = NSNumber::new_u64(id.parse()?);
        let predicate: Retained<AnyObject> = unsafe {
            msg_send![class(c"MPMediaPropertyPredicate"), predicateWithValue: &*value, forProperty: &*NSString::from_str("persistentID")]
        };
        unsafe {
            let _: () = msg_send![&*query, addFilterPredicate: &*predicate];
        }
    }
    let items: Option<Retained<NSArray<AnyObject>>> = unsafe { msg_send![&*query, items] };
    Ok(items.unwrap_or_default())
}
unsafe fn identifier(item: &AnyObject, album: bool) -> String {
    let value: u64 = if album {
        unsafe { msg_send![item, albumPersistentID] }
    } else {
        unsafe { msg_send![item, persistentID] }
    };
    value.to_string()
}
pub fn scan(root: &Path, cancelled: &dyn Fn() -> bool) -> Result<ExternalCatalog> {
    if cancelled() {
        bail!("Music library operation cancelled");
    }
    objc2::rc::autoreleasepool(|_| unsafe {
        let mut records = Vec::new();
        let artwork = BTreeMap::new();
        for item in query(None)?.to_vec() {
            if cancelled() {
                bail!("Music library operation cancelled");
            }
            let id = identifier(&item, false);
            let album_id = identifier(&item, true);
            let protected: bool = msg_send![&*item, hasProtectedAsset];
            let cloud: bool = msg_send![&*item, isCloudItem];
            let url: Option<Retained<NSURL>> = msg_send![&*item, assetURL];
            let duration: f64 = msg_send![&*item, playbackDuration];
            let disc: usize = msg_send![&*item, discNumber];
            let number: usize = msg_send![&*item, albumTrackNumber];
            let artist = fallback(text(&item, objc2::sel!(artist)), "Unknown artist");
            let compilation: bool = msg_send![&*item, isCompilation];
            records.push(Record {
                album_id: if album_id == "0" {
                    format!("item:{id}")
                } else {
                    album_id
                },
                id,
                title: fallback(text(&item, objc2::sel!(title)), "Untitled"),
                album: fallback(text(&item, objc2::sel!(albumTitle)), "Unknown album"),
                album_artist: fallback(
                    text(&item, objc2::sel!(albumArtist)),
                    if compilation {
                        "Various artists"
                    } else {
                        &artist
                    },
                ),
                artist,
                disc: disc as u32,
                number: number as u32,
                duration,
                blocked: if protected {
                    Some(PlaybackBlockReason::Protected)
                } else if cloud || url.is_none() {
                    Some(PlaybackBlockReason::NotLocal)
                } else {
                    None
                },
            });
        }
        let _ = root;
        Ok(catalog(records, artwork))
    })
}
pub fn open(adapter: &Adapter, track: &Track) -> Result<MediaInput> {
    let id = check_track(track)?.to_owned();
    if !authorized() {
        bail!("Music library access denied");
    }
    super::asset::open(adapter, move || unsafe {
        let item = query(Some(&id))?
            .firstObject()
            .context("Music item no longer available")?;
        let protected: bool = msg_send![&*item, hasProtectedAsset];
        let cloud: bool = msg_send![&*item, isCloudItem];
        if protected {
            bail!("Protected music unsupported");
        }
        if cloud {
            bail!("Not on this device");
        }
        let url: Option<Retained<NSURL>> = msg_send![&*item, assetURL];
        url.context("Not on this device")
    })
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Size {
    width: f64,
    height: f64,
}
unsafe impl Encode for Size {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[Encoding::Double, Encoding::Double]);
}
#[cfg(target_os = "ios")]
#[link(name = "UIKit", kind = "framework")]
unsafe extern "C" {
    fn UIImagePNGRepresentation(image: *const AnyObject) -> *mut objc2_foundation::NSData;
}
#[cfg(target_os = "ios")]
pub fn artwork(
    root: &Path,
    data: &ExternalCatalog,
    cancelled: &dyn Fn() -> bool,
) -> Result<BTreeMap<String, String>> {
    let mut artwork = BTreeMap::new();
    for album in &data.library.albums {
        if cancelled() || !authorized() {
            break;
        }
        objc2::rc::autoreleasepool(|_| unsafe {
            for track in &album.tracks {
                let Some(item) = query(Some(&track.id.item))
                    .ok()
                    .and_then(|items| items.firstObject())
                else {
                    continue;
                };
                let art: Option<Retained<AnyObject>> = msg_send![&*item, artwork];
                let Some(art) = art else { continue };
                let image: Option<Retained<AnyObject>> =
                    msg_send![&*art, imageWithSize: Size { width: 512.0, height: 512.0 }];
                let Some(image) = image else { continue };
                let bytes = UIImagePNGRepresentation(Retained::as_ptr(&image));
                if let Some(bytes) = bytes.as_ref()
                    && let Ok(path) = save_artwork(root, &album.id.item, bytes)
                {
                    artwork.insert(album.id.item.clone(), path);
                    break;
                }
            }
        });
    }
    Ok(artwork)
}
