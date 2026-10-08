use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AtlasTextureId, AtlasTextureKind, AtlasTile, Bounds, ContentMask, DevicePixels,
    MonochromeSprite, Point, ScaledPixels, Scene, Size, TransformationMatrix,
};

use crate::fast::adaptive::policy::{
    AtlasDamage, CANVAS_RELEASE_AFTER, CpuPlan, Decision, Frame, GpuReason, Policy, Target,
};
use crate::fast::adaptive::region::{Region, rect, whole};
use crate::fast::adaptive::{
    Adaptive, MAX_PRESENT_FAILURES, Mode, Output, Prepared, PresentMode, cpu_frames_possible,
    default_present_mode, present_mode, stats,
};
use crate::fast::cpu::atlas::AtlasMirror;
use crate::fast::cpu::raster::RasterParams;
use crate::fast::cpu::{CpuFrame, CpuPresenter};

const TARGET: Target = Target {
    width: 1000,
    height: 800,
    opaque: true,
};

fn ms(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

fn frame<'a>(
    now: Instant,
    number: u64,
    since: u64,
    damage: &'a [Bounds<DevicePixels>],
) -> Frame<'a> {
    Frame {
        now,
        target: TARGET,
        number,
        since,
        damage,
        atlas: AtlasDamage::Rects(&[]),
        needs_gpu: None,
        always: false,
    }
}

fn region(rects: &[Bounds<DevicePixels>]) -> Vec<Bounds<DevicePixels>> {
    let mut region = Region::default();
    region.add_all(rects, TARGET.bounds());
    sorted(region.rects())
}

fn sorted(rects: &[Bounds<DevicePixels>]) -> Vec<Bounds<DevicePixels>> {
    let mut rects = rects.to_vec();
    rects.sort_by_key(|rect| (rect.origin.y.0, rect.origin.x.0));
    rects
}

fn cpu(decision: Decision) -> CpuPlan {
    match decision {
        Decision::Cpu(plan) => plan,
        Decision::Gpu(reason) => panic!("drawn on the GPU: {reason:?}"),
    }
}

fn gpu(decision: Decision) -> GpuReason {
    match decision {
        Decision::Gpu(reason) => reason,
        Decision::Cpu(plan) => panic!("drawn on the CPU: {plan:?}"),
    }
}

/// A policy whose canvas shows scene 1, drawn by the GPU 200 ms before `t0`
/// (the first frame always is), then whole on the CPU at `t0`, done 5 ms
/// later.
fn with_canvas(t0: Instant) -> Policy {
    let mut policy = Policy::default();
    let before = t0 - ms(200);
    assert_eq!(
        gpu(policy.decide(&frame(before, 1, 0, &[]))),
        GpuReason::FirstFrame
    );
    assert!(!policy.gpu_drew(before, 1, 0, &[], AtlasDamage::Rects(&[])));
    let plan = cpu(policy.decide(&frame(t0, 1, 0, &[])));
    assert!(plan.whole);
    assert_eq!(plan.region.rects(), &[TARGET.bounds()]);
    policy.cpu_drew(1, TARGET, t0, t0 + ms(5), ms(5), false);
    policy
}

#[test]
fn tiny_damage_when_idle_draws_on_cpu() {
    let t0 = Instant::now();
    let mut policy = with_canvas(t0);
    let damage = [rect(10, 10, 20, 20)];
    let plan = cpu(policy.decide(&frame(t0 + ms(500), 2, 1, &damage)));
    assert!(!plan.whole);
    assert_eq!(plan.region.rects(), &damage);
    assert_eq!(plan.changed, 400);
}

#[test]
fn large_damage_in_burst_draws_on_gpu_and_small_damage_after_on_cpu() {
    let t0 = Instant::now();
    let mut policy = with_canvas(t0);
    // 300 x 300 > 1000 x 800 / 16.
    let large = [rect(0, 0, 300, 300)];
    let now = t0 + ms(5 + 10);
    assert_eq!(
        gpu(policy.decide(&frame(now, 2, 1, &large))),
        GpuReason::LargeChange
    );
    policy.gpu_drew(now + ms(2), 2, 1, &large, AtlasDamage::Rects(&[]));
    // Out of a burst, the same change draws on the CPU.
    let mut idle = with_canvas(t0);
    assert!(!cpu(idle.decide(&frame(t0 + ms(500), 2, 1, &large))).whole);

    // Still in the burst, a small change draws on the CPU with what the GPU
    // drew meanwhile.
    let small = [rect(500, 500, 10, 10)];
    let plan = cpu(policy.decide(&frame(now + ms(10), 3, 2, &small)));
    assert_eq!(sorted(plan.region.rects()), region(&[large[0], small[0]]));
    assert_eq!(plan.changed, 100);
}

#[test]
fn costly_cpu_burst_moves_to_gpu_until_a_pause() {
    let t0 = Instant::now();
    let mut policy = with_canvas(t0);
    let damage = [rect(0, 0, 50, 50)];
    // Frames every 16 ms costing 8 ms of CPU each: half the burst's time.
    let mut now = t0 + ms(20);
    let mut number = 2;
    let mut switched_at = None;
    while now < t0 + ms(1000) {
        match policy.decide(&frame(now, number, number - 1, &damage)) {
            Decision::Cpu(_) => {
                assert!(switched_at.is_none(), "back on the CPU within the burst");
                policy.cpu_drew(number, TARGET, now, now + ms(8), ms(8), false);
            }
            Decision::Gpu(reason) => {
                assert_eq!(reason, GpuReason::CpuHeavy);
                switched_at.get_or_insert(now);
                policy.gpu_drew(
                    now + ms(2),
                    number,
                    number - 1,
                    &damage,
                    AtlasDamage::Rects(&[]),
                );
            }
        }
        number += 1;
        now += ms(16);
    }
    let switched_at = switched_at.expect("the burst moved to the GPU");
    // The load counts from the canvas's frame, at t0, in the same burst.
    let lasted = switched_at - t0;
    assert!(
        lasted >= ms(250) && lasted < ms(300),
        "switched after {lasted:?}"
    );

    // A pause of 50 ms ends the burst: the CPU draws again.
    let last_end = now - ms(16) + ms(2);
    let plan = cpu(policy.decide(&frame(last_end + ms(60), number, number - 1, &damage)));
    assert!(!plan.whole);
}

#[test]
fn cheap_cpu_burst_stays_on_cpu() {
    let t0 = Instant::now();
    let mut policy = with_canvas(t0);
    let damage = [rect(0, 0, 50, 50)];
    let mut now = t0 + ms(20);
    for number in 2..100 {
        cpu(policy.decide(&frame(now, number, number - 1, &damage)));
        policy.cpu_drew(number, TARGET, now, now + ms(1), ms(2), false);
        now += ms(16);
    }
}

#[test]
fn gpu_frames_accumulate_stale_rects() {
    let t0 = Instant::now();
    let mut policy = with_canvas(t0);
    let a = rect(0, 0, 10, 10);
    let b = rect(400, 400, 10, 10);
    let c = rect(800, 100, 10, 10);
    let atlas = [rect(100, 700, 5, 5)];
    policy.gpu_drew(t0 + ms(100), 2, 1, &[a], AtlasDamage::Rects(&[]));
    policy.gpu_drew(t0 + ms(200), 3, 2, &[b], AtlasDamage::Rects(&atlas));
    assert_eq!(sorted(policy.stale().rects()), region(&[a, b, atlas[0]]));
    let plan = cpu(policy.decide(&frame(t0 + ms(500), 4, 3, &[c])));
    assert_eq!(sorted(plan.region.rects()), region(&[c, a, b, atlas[0]]));
    assert_eq!(plan.changed, 100);
    policy.cpu_drew(4, TARGET, t0 + ms(500), t0 + ms(501), ms(1), false);
    assert!(policy.stale().is_empty());

    // A GPU frame not comparable with the canvas's scene invalidates it.
    policy.gpu_drew(t0 + ms(600), 6, 5, &[a], AtlasDamage::Rects(&[]));
    assert!(!policy.has_canvas());
    assert!(cpu(policy.decide(&frame(t0 + ms(900), 7, 6, &[a]))).whole);
}

#[test]
fn canvas_released_after_a_second_of_gpu_frames() {
    let t0 = Instant::now();
    let mut policy = with_canvas(t0);
    let damage = [rect(0, 0, 10, 10)];
    let start = t0 + ms(100);
    let mut released = false;
    for (ix, number) in (2..).take(80).enumerate() {
        let now = start + ms(16 * ix as u64);
        let release = policy.gpu_drew(now, number, number - 1, &damage, AtlasDamage::Rects(&[]));
        if release {
            assert!(!released, "released once");
            assert!(now - start >= CANVAS_RELEASE_AFTER);
            released = true;
        } else {
            assert!(released || now - start < CANVAS_RELEASE_AFTER);
        }
    }
    assert!(released);
    assert!(!policy.has_canvas());
    // After a pause, the next CPU frame draws the canvas whole.
    let now = start + ms(16 * 80 + 100);
    assert!(cpu(policy.decide(&frame(now, 82, 81, &damage))).whole);
    // In a burst it would have drawn on the GPU.
    let mut policy = with_canvas(t0);
    policy.release_canvas();
    assert_eq!(
        gpu(policy.decide(&frame(t0 + ms(10), 2, 1, &damage))),
        GpuReason::WholeInBurst
    );
}

#[test]
fn since_mismatch_draws_whole() {
    let t0 = Instant::now();
    let mut policy = with_canvas(t0);
    let damage = [rect(0, 0, 10, 10)];
    // Scene 2 was never presented: scene 3's damage is relative to it.
    let plan = cpu(policy.decide(&frame(t0 + ms(500), 3, 2, &damage)));
    assert!(plan.whole);
    assert_eq!(plan.region.rects(), &[TARGET.bounds()]);
    // A resize draws whole too.
    let mut policy = with_canvas(t0);
    let mut resized = frame(t0 + ms(500), 2, 1, &damage);
    resized.target.width = 900;
    assert!(cpu(policy.decide(&resized)).whole);
    // As does damage that is not relative to anything.
    let mut policy = with_canvas(t0);
    assert!(cpu(policy.decide(&frame(t0 + ms(500), 2, 0, &damage))).whole);
    // The same scene presented again draws nothing new.
    let mut policy = with_canvas(t0);
    let plan = cpu(policy.decide(&frame(t0 + ms(500), 1, 0, &damage)));
    assert!(!plan.whole);
    assert!(plan.region.is_empty());
}

#[test]
fn atlas_writes_add_sprite_extents() {
    let t0 = Instant::now();
    let mut policy = with_canvas(t0);
    let damage = [rect(0, 0, 10, 10)];
    let sprites = [rect(300, 300, 20, 20)];
    let mut input = frame(t0 + ms(500), 2, 1, &damage);
    input.atlas = AtlasDamage::Rects(&sprites);
    let plan = cpu(policy.decide(&input));
    assert_eq!(
        sorted(plan.region.rects()),
        region(&[damage[0], sprites[0]])
    );
    assert_eq!(plan.changed, 500);
    input.atlas = AtlasDamage::Everything;
    assert!(cpu(policy.decide(&input)).whole);
}

#[test]
fn limits_and_always() {
    let t0 = Instant::now();
    let big = Target {
        width: 4000,
        height: 3000,
        opaque: false,
    };
    let mut policy = Policy::default();
    let mut input = frame(t0, 1, 0, &[]);
    input.target = big;
    input.always = true;
    assert_eq!(gpu(policy.decide(&input)), GpuReason::FirstFrame);
    policy.gpu_drew(t0 - ms(200), 1, 0, &[], AtlasDamage::Rects(&[]));
    input.always = false;
    assert_eq!(gpu(policy.decide(&input)), GpuReason::TooLarge);
    input.always = true;
    assert!(cpu(policy.decide(&input)).whole);
    input.needs_gpu = Some(GpuReason::Surfaces);
    assert_eq!(gpu(policy.decide(&input)), GpuReason::Surfaces);
    input.needs_gpu = None;
    input.number = 0;
    assert_eq!(gpu(policy.decide(&input)), GpuReason::Composition);
}

#[test]
fn half_window_region_draws_whole() {
    let t0 = Instant::now();
    let mut policy = with_canvas(t0);
    let damage = [rect(0, 0, 1000, 500)];
    let plan = cpu(policy.decide(&frame(t0 + ms(500), 2, 1, &damage)));
    assert!(plan.whole);
}

/// What a presenter was asked, in order, with the swapchain presents of GPU
/// frames (which the renderer does right after `Adaptive::gpu_drew`).
#[derive(Clone, Debug, PartialEq)]
enum Event {
    Present(Vec<Bounds<DevicePixels>>),
    Refused,
    GpuPresented,
    Released,
    SwapchainPresent,
}

#[derive(Default)]
struct Log {
    events: Vec<Event>,
    /// Frames to refuse before presenting again.
    refuse: u32,
}

impl Log {
    fn presented(&self) -> Vec<Vec<Bounds<DevicePixels>>> {
        self.events
            .iter()
            .filter_map(|event| match event {
                Event::Present(damage) => Some(sorted(damage)),
                _ => None,
            })
            .collect()
    }

    fn count(&self, wanted: &Event) -> usize {
        self.events.iter().filter(|event| *event == wanted).count()
    }
}

struct FakePresenter(Rc<RefCell<Log>>);

impl CpuPresenter for FakePresenter {
    fn present(&mut self, frame: CpuFrame<'_>) -> anyhow::Result<()> {
        let mut log = self.0.borrow_mut();
        assert_eq!(frame.pixels.len(), (frame.width * frame.height) as usize);
        assert!(frame.opaque);
        if log.refuse > 0 {
            log.refuse -= 1;
            log.events.push(Event::Refused);
            anyhow::bail!("presenter refused the frame");
        }
        log.events.push(Event::Present(frame.damage.to_vec()));
        Ok(())
    }

    fn gpu_presented(&mut self) {
        self.0.borrow_mut().events.push(Event::GpuPresented);
    }

    fn release(&mut self) {
        self.0.borrow_mut().events.push(Event::Released);
    }
}

const PARAMS: RasterParams = RasterParams {
    gamma_ratios: [0.; 4],
    grayscale_enhanced_contrast: 1.,
    subpixel_enhanced_contrast: 1.,
    is_bgr: false,
    premultiplied_alpha: false,
    dual_source_blending: true,
    path_sample_count: 4,
    fragment_bits: 12,
};

fn scene(number: u64, since: u64, damage: &[Bounds<DevicePixels>]) -> Scene {
    let mut scene = Scene::default();
    scene.damage.frame = number;
    scene.damage.since = since;
    scene.damage.rects = damage.to_vec();
    scene
}

fn scaled(x: f32, y: f32, w: f32, h: f32) -> Bounds<ScaledPixels> {
    Bounds {
        origin: Point {
            x: ScaledPixels(x),
            y: ScaledPixels(y),
        },
        size: Size {
            width: ScaledPixels(w),
            height: ScaledPixels(h),
        },
    }
}

/// A renderer's adaptive state with a fake presenter, resolved to show CPU
/// frames with it.
struct Window {
    adaptive: Adaptive,
    mirror: AtlasMirror,
    log: Rc<RefCell<Log>>,
}

impl Window {
    fn new() -> Self {
        let mut mirror = AtlasMirror::default();
        AtlasMirror::enable(&mut mirror);
        let log = Rc::new(RefCell::new(Log::default()));
        let mut adaptive = Adaptive {
            mode: Some(Mode::Auto),
            ..Adaptive::default()
        };
        Adaptive::install(
            &mut adaptive,
            Box::new(FakePresenter(log.clone())),
            &mut mirror,
        );
        adaptive.resolved = Some(Some(PresentMode::Native));
        Window {
            adaptive,
            mirror,
            log,
        }
    }

    /// Draws `scene` at `now` as `WgpuRenderer::draw` does: on the CPU, or
    /// on the GPU, presented 2 ms later right after `gpu_drew`.
    fn draw(&mut self, scene: &Scene, now: Instant) -> bool {
        let drawn = Adaptive::frame_native(
            &mut self.adaptive,
            scene,
            &mut self.mirror,
            TARGET,
            &PARAMS,
            now,
        );
        match drawn {
            Some(presented) => {
                assert!(presented);
                true
            }
            None => {
                if !self.adaptive.disabled {
                    Adaptive::gpu_drew_at(&mut self.adaptive, scene, now + ms(2));
                }
                self.log.borrow_mut().events.push(Event::SwapchainPresent);
                false
            }
        }
    }
}

#[test]
fn presenter_receives_cpu_frames_with_their_region() {
    let mut window = Window::new();
    let totals = stats::totals();
    let t0 = Instant::now();

    // The first frame draws on the GPU, the next whole on the CPU.
    assert!(!window.draw(&scene(1, 0, &[]), t0));
    assert!(window.draw(&scene(2, 1, &[]), t0 + ms(500)));
    assert_eq!(
        window.log.borrow().presented(),
        vec![vec![whole(1000, 800)]]
    );

    // A glyph rasterized into a tile the scene's sprite samples is redrawn
    // with the damage.
    let texture = AtlasTextureId {
        index: 0,
        kind: AtlasTextureKind::Monochrome,
    };
    AtlasMirror::upload_raw(
        &mut window.mirror,
        texture,
        1024,
        1024,
        1,
        rect(0, 0, 4, 4),
        &[255; 16],
    );
    let damage = [rect(0, 0, 10, 10)];
    let mut third = scene(3, 2, &damage);
    third.monochrome_sprites.push(MonochromeSprite {
        order: 0,
        pad: 0,
        bounds: scaled(100., 100., 4., 4.),
        content_mask: ContentMask {
            bounds: scaled(0., 0., 1000., 800.),
        },
        color: Default::default(),
        tile: AtlasTile {
            texture_id: texture,
            tile_id: gpui::TileId(1),
            padding: 0,
            bounds: rect(0, 0, 4, 4),
        },
        transformation: TransformationMatrix::unit(),
    });
    assert!(window.draw(&third, t0 + ms(1000)));
    assert_eq!(
        window.log.borrow().presented()[1],
        vec![rect(0, 0, 10, 10), rect(99, 99, 6, 6)]
    );

    // A large change in a burst goes to the GPU; its damage is redrawn by
    // the next CPU frame.
    let large = [rect(0, 0, 400, 400)];
    let now = t0 + ms(1010);
    assert!(!window.draw(&scene(4, 3, &large), now));
    let small = [rect(600, 600, 4, 4)];
    assert!(window.draw(&scene(5, 4, &small), now + ms(10)));
    assert_eq!(window.log.borrow().presented()[2], vec![large[0], small[0]]);

    let (cpu_frames, gpu_frames, reasons) = window.adaptive.stats.counts();
    assert_eq!((cpu_frames, gpu_frames), (3, 2));
    assert_eq!(reasons[GpuReason::LargeChange.index()], 1);
    assert_eq!(reasons[GpuReason::FirstFrame.index()], 1);
    let after = stats::totals();
    assert!(after.cpu_frames >= totals.cpu_frames + 3);
    assert!(after.cpu_frames_by_mode[0] >= totals.cpu_frames_by_mode[0] + 3);
    assert!(
        window
            .adaptive
            .stats
            .summary_for_test()
            .contains("native_n=3 ")
    );

    // A composed window's replayed scene is not numbered: GPU, and the next
    // numbered scene draws whole.
    assert!(!window.draw(&scene(0, 0, &[]), now + ms(500)));
    assert!(window.draw(&scene(7, 6, &small), now + ms(1000)));
    assert_eq!(window.log.borrow().presented()[3], vec![whole(1000, 800)]);
}

#[test]
fn gpu_presented_comes_before_the_swapchain_present() {
    let mut window = Window::new();
    let t0 = Instant::now();
    let small = [rect(0, 0, 4, 4)];
    let large = [rect(0, 0, 500, 500)];
    window.draw(&scene(1, 0, &[]), t0);
    window.draw(&scene(2, 1, &small), t0 + ms(500));
    window.draw(&scene(3, 2, &small), t0 + ms(1000));
    // In a burst: two large changes on the GPU, then a small one on the CPU.
    window.draw(&scene(4, 3, &large), t0 + ms(1010));
    window.draw(&scene(5, 4, &large), t0 + ms(1026));
    window.draw(&scene(6, 5, &small), t0 + ms(1042));
    assert_eq!(
        window.log.borrow().events,
        vec![
            // The first frame, on the GPU, tells the presenter first.
            Event::GpuPresented,
            Event::SwapchainPresent,
            Event::Present(vec![whole(1000, 800)]),
            Event::Present(small.to_vec()),
            // Once per switch to the GPU, before its present.
            Event::GpuPresented,
            Event::SwapchainPresent,
            Event::SwapchainPresent,
            Event::Present(large.to_vec()),
        ]
    );
}

#[test]
fn refused_frames_draw_on_gpu_and_keep_the_canvas_consistent() {
    let mut window = Window::new();
    let t0 = Instant::now();
    let a = [rect(0, 0, 4, 4)];
    let b = [rect(100, 100, 4, 4)];
    let c = [rect(200, 200, 4, 4)];
    window.draw(&scene(1, 0, &[]), t0);
    window.draw(&scene(2, 1, &a), t0 + ms(500));
    window.log.borrow_mut().refuse = 2;
    // Refused twice: each frame draws on the GPU, and the presenter, which
    // showed a CPU frame, is told before the first GPU present.
    assert!(!window.draw(&scene(3, 2, &b), t0 + ms(1000)));
    assert!(!window.draw(&scene(4, 3, &c), t0 + ms(1500)));
    // The next frame redraws what the GPU frames changed.
    assert!(window.draw(&scene(5, 4, &a), t0 + ms(2000)));
    let events = window.log.borrow().events.clone();
    assert_eq!(
        events[3..events.len() - 1],
        [
            Event::Refused,
            Event::GpuPresented,
            Event::SwapchainPresent,
            Event::Refused,
            Event::SwapchainPresent,
        ]
    );
    assert_eq!(
        window.log.borrow().presented().last().unwrap(),
        &sorted(&[a[0], b[0], c[0]])
    );
    assert_eq!(
        window.adaptive.stats.counts().2[GpuReason::PresentFailed.index()],
        2
    );
    assert!(!window.adaptive.disabled);
}

#[test]
fn many_refusals_in_a_row_turn_the_cpu_path_off() {
    let mut window = Window::new();
    let t0 = Instant::now();
    window.draw(&scene(1, 0, &[]), t0);
    window.log.borrow_mut().refuse = u32::MAX;
    let small = [rect(0, 0, 4, 4)];
    let mut now = t0;
    let last = 1 + MAX_PRESENT_FAILURES as u64;
    for number in 2..=last {
        now += ms(500);
        assert!(!window.draw(&scene(number, number - 1, &small), now));
        assert_eq!(window.adaptive.disabled, number == last);
    }
    assert!(!AtlasMirror::is_enabled(&window.mirror));
    assert_eq!(
        window.log.borrow().count(&Event::Refused),
        MAX_PRESENT_FAILURES as usize
    );
    // Off for good: no more frames reach the presenter.
    window.log.borrow_mut().refuse = 0;
    assert!(!window.draw(&scene(last + 1, last, &small), now + ms(500)));
    assert!(window.log.borrow().presented().is_empty());
    assert!(window.adaptive.presenter.is_none());
}

#[test]
fn canvas_and_presenter_buffers_released_after_gpu_second() {
    let mut window = Window::new();
    let t0 = Instant::now();
    window.draw(&scene(1, 0, &[]), t0);
    assert!(window.draw(&scene(2, 1, &[]), t0 + ms(500)));
    assert!(window.adaptive.canvas.is_some());
    // Animating most of the window, on the GPU.
    let large = [rect(0, 0, 500, 500)];
    for ix in 0..70u64 {
        let number = ix + 3;
        assert!(!window.draw(&scene(number, number - 1, &large), t0 + ms(510 + 16 * ix)));
    }
    assert!(window.adaptive.canvas.is_none());
    assert_eq!(window.log.borrow().count(&Event::Released), 1);
}

#[test]
fn missing_atlas_textures_need_gpu() {
    let mut window = Window::new();
    AtlasMirror::disable(&mut window.mirror);
    let texture = AtlasTextureId {
        index: 2,
        kind: AtlasTextureKind::Polychrome,
    };
    // Uploaded while the mirror was off.
    AtlasMirror::upload_raw(
        &mut window.mirror,
        texture,
        1024,
        1024,
        4,
        rect(0, 0, 1, 1),
        &[0; 4],
    );
    AtlasMirror::enable(&mut window.mirror);
    let mut second = scene(2, 1, &[]);
    second.polychrome_sprites.push(gpui::PolychromeSprite {
        order: 0,
        pad: 0,
        grayscale: Default::default(),
        opacity: 1.,
        bounds: scaled(0., 0., 1., 1.),
        content_mask: ContentMask {
            bounds: scaled(0., 0., 10., 10.),
        },
        corner_radii: Default::default(),
        tile: AtlasTile {
            texture_id: texture,
            tile_id: gpui::TileId(1),
            padding: 0,
            bounds: rect(0, 0, 1, 1),
        },
    });
    let t0 = Instant::now();
    window.draw(&scene(1, 0, &[]), t0);
    assert!(!window.draw(&second, t0 + ms(500)));
    assert_eq!(
        window.adaptive.stats.counts().2[GpuReason::MissingAtlas.index()],
        1
    );
}

#[test]
fn present_modes_resolve_from_the_setting_and_the_presenter() {
    if !cpu_frames_possible() {
        return;
    }
    let native = Some(PresentMode::Native);
    let blit = Some(PresentMode::Blit);
    assert_eq!(present_mode(blit, true), blit);
    assert_eq!(present_mode(blit, false), blit);
    assert_eq!(present_mode(native, true), native);
    assert_eq!(present_mode(native, false), blit);
    assert_eq!(present_mode(None, true), default_present_mode(true));
    assert_eq!(present_mode(None, false), default_present_mode(false));
}

/// An output standing for the blit path's surface.
struct FakeOutput {
    prepared: Prepared,
    shown: Vec<Vec<Bounds<DevicePixels>>>,
}

impl Output for FakeOutput {
    fn prepare(&mut self, _target: Target) -> Prepared {
        self.prepared
    }

    fn show(&mut self, frame: CpuFrame<'_>) -> anyhow::Result<()> {
        self.shown.push(frame.damage.to_vec());
        Ok(())
    }
}

fn blit_frame(
    window: &mut Window,
    output: &mut FakeOutput,
    number: u64,
    now: Instant,
) -> Option<bool> {
    Adaptive::frame(
        &mut window.adaptive,
        &scene(number, number - 1, &[]),
        &mut window.mirror,
        TARGET,
        &PARAMS,
        now,
        output,
        PresentMode::Blit,
    )
}

#[test]
fn unavailable_blit_surface_draws_on_gpu_or_skips_the_frame() {
    let mut window = Window::new();
    window.adaptive.resolved = Some(Some(PresentMode::Blit));
    window.adaptive.presenter_shows = false;
    let t0 = Instant::now();
    let mut output = FakeOutput {
        prepared: Prepared::Ready,
        shown: Vec::new(),
    };
    assert_eq!(blit_frame(&mut window, &mut output, 1, t0), None);
    Adaptive::gpu_drew_at(&mut window.adaptive, &scene(1, 0, &[]), t0 + ms(2));

    output.prepared = Prepared::Gpu;
    assert_eq!(blit_frame(&mut window, &mut output, 2, t0 + ms(500)), None);
    assert_eq!(
        window.adaptive.stats.counts().2[GpuReason::SurfaceUnavailable.index()],
        1
    );
    Adaptive::gpu_drew_at(&mut window.adaptive, &scene(2, 1, &[]), t0 + ms(502));
    output.prepared = Prepared::NotPresented;
    assert_eq!(
        blit_frame(&mut window, &mut output, 3, t0 + ms(1000)),
        Some(false)
    );
    output.prepared = Prepared::Ready;
    // Scene 3 was not shown: scene 4 draws whole.
    assert_eq!(
        blit_frame(&mut window, &mut output, 4, t0 + ms(1500)),
        Some(true)
    );
    assert_eq!(output.shown, vec![vec![whole(1000, 800)]]);
    // The blit path never tells the platform's presenter.
    Adaptive::gpu_drew_at(&mut window.adaptive, &scene(5, 4, &[]), t0 + ms(2000));
    assert_eq!(window.log.borrow().count(&Event::GpuPresented), 0);
    assert!(
        window
            .adaptive
            .stats
            .summary_for_test()
            .contains("blit_n=1 ")
    );
}
