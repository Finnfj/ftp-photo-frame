use std::thread::{self, JoinHandle};

pub use image::{open, DynamicImage};

use image::{
    self, imageops::{self}, GenericImageView
};

use crate::{cli::Rotation, error::ErrorToString};
use fast_image_resize::*;

pub trait Framed {
    /// Resizes an image while preserving the aspect ratio, and centers it on screen. Returns a new
    /// image that exactly matches the screen size
    fn fit_to_screen(&self, screen_size: (u32, u32), rotation: Rotation) -> Self;

    /// Resizes an image while preserving the aspect ratio, and centers it on screen, filling any
    /// empty space with blurred background
    fn fit_to_screen_and_add_background(&self, screen_size: (u32, u32), rotation: Rotation)
        -> Self;

    /// Adds update icon to an image
    fn overlay_update_icon(&mut self, update_icon: &Self, rotation: Rotation);

    fn resize(&self, new_width: u32, new_height: u32) -> Self;

    fn rotate(&self, degrees: Rotation) -> Self;
}

impl Framed for DynamicImage {
    fn fit_to_screen(&self, screen_size: (u32, u32), rotation: Rotation) -> Self {
        let rotated = self.rotate(rotation);
        let resized = resize_to_fit_screen(&rotated, screen_size);
        center_on_screen(&resized, screen_size)
    }

    fn fit_to_screen_and_add_background(&self, screen_size: (u32, u32), rotate: Rotation) -> Self {
        internal_fit_to_screen_and_add_background(
            self,
            screen_size,
            rotate,
            brighten_and_blur_background,
        )
    }

    fn overlay_update_icon(&mut self, update_icon: &Self, rotation: Rotation) {
        let (width, height) = (self.width(), self.height());
        let (x_offset, y_offset) = match rotation {
            Rotation::D0 => (0, 0),
            Rotation::D90 => (width - update_icon.height(), 0),
            Rotation::D180 => (width - update_icon.width(), height - update_icon.height()),
            Rotation::D270 => (0, height - update_icon.width()),
        };
        imageops::overlay(self, update_icon, x_offset as i64, y_offset as i64);
    }

    fn resize(&self, new_width: u32, new_height: u32) -> Self {
        log::info!("Resizing image: to {}x{}", new_width, new_height);
        let start = std::time::Instant::now();

        let mut resized_image = DynamicImage::new(new_width, new_height, self.color());
        let mut resizer = fast_image_resize::Resizer::new();
        resizer.resize(self, &mut resized_image, &fast_image_resize::ResizeOptions::new().resize_alg(ResizeAlg::Convolution(fast_image_resize::FilterType::Lanczos3)).fit_into_destination(None)).unwrap();
        
        log::info!("Resizing took: {:?}", start.elapsed());
        return resized_image;
    }

    fn rotate(&self, degrees: Rotation) -> Self {
        match degrees {
            Rotation::D0 => self.to_owned(),
            Rotation::D90 => self.rotate90(),
            Rotation::D180 => self.rotate180(),
            Rotation::D270 => self.rotate270(),
        }
    }
}

pub fn load_from_memory(buffer: &[u8]) -> Result<DynamicImage, String> {
    image::load_from_memory(buffer).map_err_to_string()
}

/// Testable version of [Framed::fit_to_screen_and_add_background]
fn internal_fit_to_screen_and_add_background(
    original: &DynamicImage,
    screen_size: (u32, u32),
    rotate: Rotation,
    brighten_and_blur: fn(&DynamicImage) -> DynamicImage,
) -> DynamicImage {
    let rotated = original.rotate(rotate);
    if rotated.dimensions() == screen_size {
        return rotated;
    }

    let (bg_thread1, bg_thread2) =
        background_fill_threads(&rotated, screen_size, brighten_and_blur);
    let foreground = resize_to_fit_screen(&rotated, screen_size);
    if foreground.dimensions() == screen_size {
        return foreground;
    }

    let (x_res, y_res) = screen_size;
    let mut final_image = DynamicImage::new_rgb8(x_res, y_res);

    let bg_fill_1 = bg_thread1.join().unwrap();
    imageops::overlay(&mut final_image, &bg_fill_1, 0, 0);

    let bg_fill_2 = bg_thread2.join().unwrap();
    imageops::overlay(
        &mut final_image,
        &bg_fill_2,
        (x_res - bg_fill_2.width()) as i64,
        (y_res - bg_fill_2.height()) as i64,
    );

    let (w_diff, h_diff) = Dimensions::from(screen_size).diff(foreground.dimensions().into());
    imageops::overlay(
        &mut final_image,
        &foreground,
        (w_diff / 2.0).round() as i64,
        (h_diff / 2.0).round() as i64,
    );

    final_image
}

fn resize_to_fit_screen(original: &DynamicImage, (x_res, y_res): (u32, u32)) -> DynamicImage {
    let original_dimensions = Dimensions::from(original.dimensions());
    let screen_dimensions = Dimensions::from((x_res, y_res));
    let foreground_dimensions = original_dimensions.resize(screen_dimensions);

    let (new_width, new_height) = foreground_dimensions.into();
    Framed::resize(original, new_width, new_height)
}

fn center_on_screen(original: &DynamicImage, (x_res, y_res): (u32, u32)) -> DynamicImage {
    let original_dimensions = Dimensions::from(original.dimensions());
    let screen_dimensions = Dimensions::from((x_res, y_res));
    let foreground_dimensions = original_dimensions.resize(screen_dimensions);

    let mut final_image = DynamicImage::new_rgb8(x_res, y_res);

    let (w_diff, h_diff) = screen_dimensions.diff(foreground_dimensions);
    imageops::overlay(
        &mut final_image,
        original,
        (w_diff / 2.0).round() as i64,
        (h_diff / 2.0).round() as i64,
    );

    final_image
}

fn background_fill_threads(
    image: &DynamicImage,
    (x_res, y_res): (u32, u32),
    brighten_and_blur: fn(&DynamicImage) -> DynamicImage,
) -> (JoinHandle<DynamicImage>, JoinHandle<DynamicImage>) {
    let original_dimensions = Dimensions::from(image.dimensions());
    let screen_dimensions = Dimensions::from((x_res, y_res));
    let (
        Coords {
            x: x1,
            y: y1,
            w: w1,
            h: h1,
        },
        Coords {
            x: x2,
            y: y2,
            w: w2,
            h: h2,
        },
    ) = original_dimensions.background_crops(screen_dimensions);
    let (bg_crop1, bg_crop2) = (
        image.crop_imm(
            x1.floor() as u32,
            y1.floor() as u32,
            w1.ceil() as u32,
            h1.ceil() as u32,
        ),
        image.crop_imm(
            x2.floor() as u32,
            y2.floor() as u32,
            w2.ceil() as u32,
            h2.ceil() as u32,
        ),
    );
    let bg_thread1 = thread::spawn(move || {
        let mut bg = DynamicImage::new(x_res, y_res, bg_crop1.color());
        let mut resizer = fast_image_resize::Resizer::new();
        resizer.resize(&bg_crop1, &mut bg, &fast_image_resize::ResizeOptions::new().resize_alg(ResizeAlg::Nearest).fit_into_destination(None)).unwrap();
        brighten_and_blur(&bg)
    });
    let bg_thread2 = thread::spawn(move || {
        let mut bg = DynamicImage::new(x_res, y_res, bg_crop2.color());
        let mut resizer = fast_image_resize::Resizer::new();
        resizer.resize(&bg_crop2, &mut bg, &fast_image_resize::ResizeOptions::new().resize_alg(ResizeAlg::Nearest).fit_into_destination(None)).unwrap();
        brighten_and_blur(&bg)
    });
    (bg_thread1, bg_thread2)
}

fn brighten_and_blur_background(background: &DynamicImage) -> DynamicImage {
    const BRIGHTNESS_OFFSET: i32 = -20;
    const BLUR_SIGMA: f32 = 45.0;
    background.brighten(BRIGHTNESS_OFFSET).blur(BLUR_SIGMA)
}

#[derive(Debug, Clone, Copy)]
struct Dimensions {
    w: f64,
    h: f64,
}

impl From<(u32, u32)> for Dimensions {
    fn from((w, h): (u32, u32)) -> Self {
        Self {
            w: w as f64,
            h: h as f64,
        }
    }
}

impl Into<(u32, u32)> for Dimensions {
    fn into(self) -> (u32, u32) {
        (self.w as u32, self.h as u32)
    }
}

impl Dimensions {
    const fn new(w: f64, h: f64) -> Self {
        Self { w, h }
    }

    fn diff(self, Dimensions { w, h }: Dimensions) -> (f64, f64) {
        (f64::abs(self.w - w), f64::abs(self.h - h))
    }

    /// Resize dimensions preserving aspect ratio. The dimensions are scaled to the maximum possible
    /// size that fits within the bounds specified by `new_width` and `new_height`.
    fn resize(
        self,
        Dimensions {
            w: new_width,
            h: new_height,
        }: Dimensions,
    ) -> Dimensions {
        let w_ratio = new_width / self.w;
        let h_ratio = new_height / self.h;

        let ratio = f64::min(w_ratio, h_ratio);

        let nw = f64::max(self.w * ratio, 1.0);
        let nh = f64::max(self.h * ratio, 1.0);

        Dimensions::new(nw, nh)
    }

    /// Calculates coordinates of parts of the foreground that will form the background fills.
    fn background_crops(self, screen_size: Dimensions) -> (Coords, Coords) {
        let screen_to_image_projection = screen_size.resize(self);
        let (w_diff, h_diff) = screen_to_image_projection.diff(self);
        let (bg_x, bg_y) = (w_diff / 2.0, h_diff / 2.0);

        let image_to_projected_screen = self.resize(screen_to_image_projection);
        let (w_diff, h_diff) = image_to_projected_screen.diff(screen_to_image_projection);

        let Dimensions {
            w: screen_w,
            h: screen_h,
        } = screen_to_image_projection;

        if w_diff > 0.0 {
            /* Needs background on left and right. */
            let bg_w = w_diff / 2.0;
            (
                Coords {
                    x: bg_x,
                    y: bg_y,
                    w: bg_w,
                    h: screen_h,
                },
                Coords {
                    x: self.w - bg_w,
                    y: bg_y,
                    w: bg_w,
                    h: screen_h,
                },
            )
        } else {
            /* Needs background on top and bottom .*/
            let bg_h = h_diff / 2.0;
            (
                Coords {
                    x: bg_x,
                    y: bg_y,
                    w: screen_w,
                    h: bg_h,
                },
                Coords {
                    x: bg_x,
                    y: self.h - bg_h,
                    w: screen_w,
                    h: bg_h,
                },
            )
        }
    }
}

#[derive(Debug)]
struct Coords {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[cfg(test)]
mod tests {
    use crate::cli::Rotation;
    use image::{GenericImage, GenericImageView, Rgba};

    use super::*;

    const RED: Rgba<u8> = Rgba([255, 0, 0, 255]);
    const GREEN: Rgba<u8> = Rgba([0, 255, 0, 255]);
    const BLUE: Rgba<u8> = Rgba([0, 0, 255, 255]);

    #[test]
    fn when_smaller_image_fits_perfectly_then_background_is_not_created() {
        let pixel = Rgba([1, 2, 3, 255]);
        let original = create_test_image((60, 40), pixel);
        let screen = (120, 80);

        let result = internal_fit_to_screen_and_add_background(
            &original,
            screen,
            Rotation::D0,
            panicking_brighten_and_blur_stub,
        );

        assert_eq!(result.pixels().count(), 120 * 80);
        assert!(result.pixels().all(|(_, _, p)| p == pixel));
    }

    #[test]
    fn when_larger_image_fits_perfectly_then_background_is_not_created() {
        let pixel = Rgba([1, 2, 3, 255]);
        let original = create_test_image((759, 426), pixel);
        let screen = (640, 360);

        let result = internal_fit_to_screen_and_add_background(
            &original,
            screen,
            Rotation::D0,
            panicking_brighten_and_blur_stub,
        );

        assert_eq!(result.pixels().count(), 640 * 360);
        assert!(result.pixels().all(|(_, _, p)| p == pixel));
    }

    #[test]
    fn when_smaller_image_fits_vertically_then_background_fills_left_and_right_space() {
        let mut original = create_test_image((50, 40), RED);
        for y in 3..37 {
            /* Color the part that forms left background green */
            for x in 0..6 {
                original.put_pixel(x, y, GREEN);
            }
            /* Color the part that forms right background blue */
            for x in 44..50 {
                original.put_pixel(x, y, BLUE);
            }
        }
        let (x_res, y_res) = (120, 80); /* screen resolution */
        fn brighten_and_blur_stub(img: &DynamicImage) -> DynamicImage {
            /* This will help asserting that the function got applied to the background */
            img.brighten(-55)
        }

        let result = internal_fit_to_screen_and_add_background(
            &original,
            (x_res, y_res),
            Rotation::D0,
            brighten_and_blur_stub,
        );

        assert_eq!(result.pixels().count(), (x_res * y_res) as usize);
        let expected_bg_w = 10;
        for y in 0..y_res {
            /* Check left background fill is green, brightened by -55 */
            for x in 0..expected_bg_w {
                assert_eq!(result.get_pixel(x, y), Rgba([0, 200, 0, 255]));
            }
            /* Check right background fill is blue, brightened by -55 */
            for x in x_res - expected_bg_w..x_res {
                assert_eq!(result.get_pixel(x, y), Rgba([0, 0, 200, 255]))
            }
        }
    }

    #[test]
    fn when_bigger_image_fits_vertically_then_background_fills_left_and_right_space() {
        let mut original = create_test_image((100, 80), RED);
        for y in 6..74 {
            /* Color the part that forms left background green */
            for x in 0..12 {
                original.put_pixel(x, y, GREEN);
            }
            /* Color the part that forms right background blue */
            for x in 88..100 {
                original.put_pixel(x, y, BLUE);
            }
        }
        let (x_res, y_res) = (60, 40); /* screen resolution */
        fn brighten_and_blur_stub(img: &DynamicImage) -> DynamicImage {
            /* This will help asserting that the function got applied to the background */
            img.brighten(-55)
        }

        let result = internal_fit_to_screen_and_add_background(
            &original,
            (x_res, y_res),
            Rotation::D0,
            brighten_and_blur_stub,
        );

        assert_eq!(result.pixels().count(), (x_res * y_res) as usize);
        let expected_bg_w = 5;
        for y in 0..y_res {
            /* Check left background fill is green, brightened by -55 */
            for x in 0..expected_bg_w {
                assert_eq!(result.get_pixel(x, y), Rgba([0, 200, 0, 255]));
            }
            /* Check right background fill is blue, brightened by -55 */
            for x in x_res - expected_bg_w..x_res {
                assert_eq!(result.get_pixel(x, y), Rgba([0, 0, 200, 255]))
            }
        }
    }

    #[test]
    fn when_smaller_image_fits_horizontally_then_background_fills_top_and_bottom_space() {
        let mut original = create_test_image((60, 30), RED);
        for x in 7..53 {
            /* Color the part that forms top background green */
            for y in 0..4 {
                original.put_pixel(x, y, GREEN);
            }
            /* Color the part that forms bottom background blue */
            for y in 26..30 {
                original.put_pixel(x, y, BLUE);
            }
        }
        let (x_res, y_res) = (120, 80); /* screen resolution */
        fn brighten_and_blur_stub(img: &DynamicImage) -> DynamicImage {
            /* This will help asserting that the function got applied to the background */
            img.brighten(-55)
        }

        let result = internal_fit_to_screen_and_add_background(
            &original,
            (x_res, y_res),
            Rotation::D0,
            brighten_and_blur_stub,
        );

        assert_eq!(result.pixels().count(), (x_res * y_res) as usize);
        let expected_bg_h = 10;
        for x in 0..x_res {
            /* Check top background fill is green, brightened by -55 */
            for y in 0..expected_bg_h {
                assert_eq!(result.get_pixel(x, y), Rgba([0, 200, 0, 255]));
            }
            /* Check bottom background fill is blue, brightened by -55 */
            for y in y_res - expected_bg_h..y_res {
                assert_eq!(result.get_pixel(x, y), Rgba([0, 0, 200, 255]))
            }
        }
    }

    #[test]
    fn when_bigger_image_fits_horizontally_then_background_fills_top_and_bottom_space() {
        let mut original = create_test_image((120, 60), RED);
        for x in 14..106 {
            /* Color the part that forms top background green */
            for y in 0..8 {
                original.put_pixel(x, y, GREEN);
            }
            /* Color the part that forms bottom background blue */
            for y in 52..60 {
                original.put_pixel(x, y, BLUE);
            }
        }
        let (x_res, y_res) = (60, 40); /* screen resolution */
        fn brighten_and_blur_stub(img: &DynamicImage) -> DynamicImage {
            /* This will help asserting that the function got applied to the background */
            img.brighten(-55)
        }

        let result = internal_fit_to_screen_and_add_background(
            &original,
            (x_res, y_res),
            Rotation::D0,
            brighten_and_blur_stub,
        );

        assert_eq!(result.pixels().count(), (x_res * y_res) as usize);
        let expected_bg_h = 5;
        for x in 0..x_res {
            /* Check top background fill is green, brightened by -55 */
            for y in 0..expected_bg_h {
                assert_eq!(result.get_pixel(x, y), Rgba([0, 200, 0, 255]));
            }
            /* Check bottom background fill is blue, brightened by -55 */
            for y in y_res - expected_bg_h..y_res {
                assert_eq!(result.get_pixel(x, y), Rgba([0, 0, 200, 255]))
            }
        }
    }

    fn create_test_image((w, h): (u32, u32), pixel: Rgba<u8>) -> DynamicImage {
        let mut image = DynamicImage::new_rgb8(w, h);
        for y in 0..h {
            for x in 0..w {
                image.put_pixel(x, y, pixel);
            }
        }
        image
    }

    fn panicking_brighten_and_blur_stub(_: &DynamicImage) -> DynamicImage {
        panic!("Unexpected creation of background when image fits perfectly");
    }
}
