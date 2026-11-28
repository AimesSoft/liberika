#import <AppKit/AppKit.h>
#import <QuartzCore/QuartzCore.h>
#import <math.h>

extern void erika_demo_attach_layer(void *layer, unsigned int width, unsigned int height, double scale);
extern void erika_demo_resize_layer(unsigned int width, unsigned int height, double scale);
extern void erika_demo_set_display_headroom(float headroom, bool known);
extern void erika_demo_render_frame(double time_seconds);
extern void erika_demo_toggle_play_pause(void);
extern void erika_demo_seek_seconds(double seconds);
extern double erika_demo_position_seconds(void);
extern double erika_demo_duration_seconds(void);
extern bool erika_demo_is_playing(void);
extern double erika_demo_smoke_seconds(void);

/// Potential EDR headroom of a screen: what it can do regardless of the
/// current brightness setting. 1.0 means no EDR. Main thread only.
static float ErikaPotentialEdrHeadroom(NSScreen *screen) {
  if (screen == nil) {
    return 1.0f;
  }
  SEL selector = NSSelectorFromString(@"maximumPotentialExtendedDynamicRangeColorComponentValue");
  if (![screen respondsToSelector:selector]) {
    return 1.0f;
  }
  NSNumber *value = [screen valueForKey:@"maximumPotentialExtendedDynamicRangeColorComponentValue"];
  if (![value isKindOfClass:[NSNumber class]]) {
    return 1.0f;
  }
  return MAX(1.0f, value.floatValue);
}

static NSString *ErikaFormatTime(double seconds) {
  if (!isfinite(seconds) || seconds < 0.0) {
    seconds = 0.0;
  }
  NSInteger total = (NSInteger)llround(seconds);
  NSInteger hours = total / 3600;
  NSInteger minutes = (total / 60) % 60;
  NSInteger secs = total % 60;
  if (hours > 0) {
    return [NSString stringWithFormat:@"%ld:%02ld:%02ld", (long)hours, (long)minutes, (long)secs];
  }
  return [NSString stringWithFormat:@"%ld:%02ld", (long)minutes, (long)secs];
}

@interface ErikaMetalDemoView : NSView
@property(nonatomic, strong) CAMetalLayer *metalLayer;
@property(nonatomic, strong) NSTimer *timer;
@property(nonatomic, assign) CFTimeInterval startTime;
@end

@implementation ErikaMetalDemoView

- (instancetype)initWithFrame:(NSRect)frameRect {
  self = [super initWithFrame:frameRect];
  if (self) {
    self.wantsLayer = YES;
    self.metalLayer = [CAMetalLayer layer];
    self.metalLayer.pixelFormat = MTLPixelFormatBGRA8Unorm;
    self.metalLayer.framebufferOnly = YES;
    self.metalLayer.opaque = YES;
    self.metalLayer.delegate = (id<CALayerDelegate>)self;
    self.layer = self.metalLayer;
    self.startTime = CACurrentMediaTime();
  }
  return self;
}

- (BOOL)wantsUpdateLayer {
  return YES;
}

- (void)viewDidMoveToWindow {
  [super viewDidMoveToWindow];
  [self updateDrawableSizeAndAttach:YES];
  if (self.window != nil && self.timer == nil) {
    self.timer = [NSTimer scheduledTimerWithTimeInterval:(1.0 / 60.0)
                                                  target:self
                                                selector:@selector(renderTick:)
                                                userInfo:nil
                                                 repeats:YES];
    [[NSRunLoop mainRunLoop] addTimer:self.timer forMode:NSRunLoopCommonModes];
  }
}

- (void)viewWillMoveToWindow:(NSWindow *)newWindow {
  if (newWindow == nil) {
    [self.timer invalidate];
    self.timer = nil;
  }
  [super viewWillMoveToWindow:newWindow];
}

- (void)setFrameSize:(NSSize)newSize {
  [super setFrameSize:newSize];
  [self updateDrawableSizeAndAttach:NO];
}

- (void)viewDidChangeBackingProperties {
  [super viewDidChangeBackingProperties];
  [self updateDrawableSizeAndAttach:NO];
}

- (void)updateDrawableSizeAndAttach:(BOOL)attach {
  CGFloat scale = self.window.backingScaleFactor > 0 ? self.window.backingScaleFactor : NSScreen.mainScreen.backingScaleFactor;
  CGSize drawableSize = CGSizeMake(MAX(1.0, self.bounds.size.width * scale), MAX(1.0, self.bounds.size.height * scale));
  self.metalLayer.drawableSize = drawableSize;
  self.metalLayer.frame = self.bounds;
  unsigned int pixelWidth = (unsigned int)MAX(1.0, round(drawableSize.width));
  unsigned int pixelHeight = (unsigned int)MAX(1.0, round(drawableSize.height));
  // The renderer negotiates its output mode off the main thread and no longer
  // reads AppKit itself, so publish the presenting screen's EDR capability
  // from here: on attach, and again whenever the window moves to a screen with
  // different backing properties.
  erika_demo_set_display_headroom(ErikaPotentialEdrHeadroom(self.window.screen), true);
  if (attach) {
    erika_demo_attach_layer((__bridge void *)self.metalLayer, pixelWidth, pixelHeight, scale);
  } else {
    erika_demo_resize_layer(pixelWidth, pixelHeight, scale);
  }
}

- (void)renderTick:(NSTimer *)timer {
  (void)timer;
  double elapsed = CACurrentMediaTime() - self.startTime;
  erika_demo_render_frame(elapsed);
}

@end

@interface ErikaControlsView : NSView
@property(nonatomic, strong) NSButton *playPauseButton;
@property(nonatomic, strong) NSSlider *progressSlider;
@property(nonatomic, strong) NSTextField *timeLabel;
@property(nonatomic, strong) NSTimer *timer;
@property(nonatomic, assign) BOOL scrubbing;
@end

@implementation ErikaControlsView

- (instancetype)initWithFrame:(NSRect)frameRect {
  self = [super initWithFrame:frameRect];
  if (self) {
    self.wantsLayer = YES;
    self.layer.backgroundColor = [NSColor colorWithWhite:0.08 alpha:1.0].CGColor;

    self.playPauseButton = [NSButton buttonWithTitle:@"Pause" target:self action:@selector(togglePlayPause:)];
    self.playPauseButton.bezelStyle = NSBezelStyleRegularSquare;
    self.playPauseButton.bordered = NO;
    self.playPauseButton.wantsLayer = YES;
    self.playPauseButton.layer.backgroundColor = [NSColor colorWithWhite:0.18 alpha:1.0].CGColor;
    self.playPauseButton.layer.cornerRadius = 4.0;
    self.playPauseButton.contentTintColor = NSColor.whiteColor;
    self.playPauseButton.translatesAutoresizingMaskIntoConstraints = NO;
    [self addSubview:self.playPauseButton];

    self.progressSlider = [[NSSlider alloc] initWithFrame:NSZeroRect];
    self.progressSlider.minValue = 0.0;
    self.progressSlider.maxValue = 1.0;
    self.progressSlider.doubleValue = 0.0;
    self.progressSlider.continuous = YES;
    self.progressSlider.target = self;
    self.progressSlider.action = @selector(sliderChanged:);
    self.progressSlider.translatesAutoresizingMaskIntoConstraints = NO;
    [self addSubview:self.progressSlider];

    self.timeLabel = [NSTextField labelWithString:@"0:00 / 0:00"];
    self.timeLabel.textColor = NSColor.whiteColor;
    self.timeLabel.alignment = NSTextAlignmentRight;
    self.timeLabel.font = [NSFont monospacedDigitSystemFontOfSize:12.0 weight:NSFontWeightRegular];
    self.timeLabel.translatesAutoresizingMaskIntoConstraints = NO;
    [self addSubview:self.timeLabel];

    [NSLayoutConstraint activateConstraints:@[
      [self.playPauseButton.leadingAnchor constraintEqualToAnchor:self.leadingAnchor constant:12.0],
      [self.playPauseButton.centerYAnchor constraintEqualToAnchor:self.centerYAnchor],
      [self.playPauseButton.widthAnchor constraintEqualToConstant:76.0],
      [self.progressSlider.leadingAnchor constraintEqualToAnchor:self.playPauseButton.trailingAnchor constant:12.0],
      [self.progressSlider.trailingAnchor constraintEqualToAnchor:self.timeLabel.leadingAnchor constant:-12.0],
      [self.progressSlider.centerYAnchor constraintEqualToAnchor:self.centerYAnchor],
      [self.timeLabel.trailingAnchor constraintEqualToAnchor:self.trailingAnchor constant:-12.0],
      [self.timeLabel.centerYAnchor constraintEqualToAnchor:self.centerYAnchor],
      [self.timeLabel.widthAnchor constraintEqualToConstant:118.0],
    ]];
  }
  return self;
}

- (void)viewDidMoveToWindow {
  [super viewDidMoveToWindow];
  if (self.window != nil && self.timer == nil) {
    self.timer = [NSTimer scheduledTimerWithTimeInterval:0.25
                                                  target:self
                                                selector:@selector(refreshControls:)
                                                userInfo:nil
                                                 repeats:YES];
    [[NSRunLoop mainRunLoop] addTimer:self.timer forMode:NSRunLoopCommonModes];
    [self refreshControls:nil];
  }
}

- (void)viewWillMoveToWindow:(NSWindow *)newWindow {
  if (newWindow == nil) {
    [self.timer invalidate];
    self.timer = nil;
  }
  [super viewWillMoveToWindow:newWindow];
}

- (void)togglePlayPause:(id)sender {
  (void)sender;
  erika_demo_toggle_play_pause();
  [self refreshControls:nil];
}

- (void)sliderChanged:(NSSlider *)sender {
  double duration = erika_demo_duration_seconds();
  if (duration <= 0.0 || !isfinite(duration)) {
    return;
  }
  self.scrubbing = YES;
  erika_demo_seek_seconds(sender.doubleValue);
  [self refreshControls:nil];
  self.scrubbing = NO;
}

- (void)refreshControls:(NSTimer *)timer {
  (void)timer;
  double duration = erika_demo_duration_seconds();
  double position = erika_demo_position_seconds();
  BOOL hasDuration = duration > 0.0 && isfinite(duration);
  if (hasDuration) {
    self.progressSlider.enabled = YES;
    self.progressSlider.maxValue = duration;
    if (!self.scrubbing) {
      self.progressSlider.doubleValue = MIN(MAX(position, 0.0), duration);
    }
  } else {
    self.progressSlider.enabled = NO;
    self.progressSlider.maxValue = 1.0;
    self.progressSlider.doubleValue = 0.0;
  }
  self.playPauseButton.title = erika_demo_is_playing() ? @"Pause" : @"Play";
  self.timeLabel.stringValue = [NSString stringWithFormat:@"%@ / %@", ErikaFormatTime(position), ErikaFormatTime(duration)];
}

@end

@interface ErikaPlayerContainerView : NSView
@property(nonatomic, strong) ErikaMetalDemoView *videoView;
@property(nonatomic, strong) ErikaControlsView *controlsView;
@end

@implementation ErikaPlayerContainerView

- (instancetype)initWithFrame:(NSRect)frameRect {
  self = [super initWithFrame:frameRect];
  if (self) {
    self.wantsLayer = YES;
    self.layer.backgroundColor = NSColor.blackColor.CGColor;

    self.videoView = [[ErikaMetalDemoView alloc] initWithFrame:NSZeroRect];
    self.videoView.translatesAutoresizingMaskIntoConstraints = NO;
    [self addSubview:self.videoView];

    self.controlsView = [[ErikaControlsView alloc] initWithFrame:NSZeroRect];
    self.controlsView.translatesAutoresizingMaskIntoConstraints = NO;
    [self addSubview:self.controlsView];

    [NSLayoutConstraint activateConstraints:@[
      [self.videoView.leadingAnchor constraintEqualToAnchor:self.leadingAnchor],
      [self.videoView.trailingAnchor constraintEqualToAnchor:self.trailingAnchor],
      [self.videoView.topAnchor constraintEqualToAnchor:self.topAnchor],
      [self.videoView.bottomAnchor constraintEqualToAnchor:self.controlsView.topAnchor],
      [self.controlsView.leadingAnchor constraintEqualToAnchor:self.leadingAnchor],
      [self.controlsView.trailingAnchor constraintEqualToAnchor:self.trailingAnchor],
      [self.controlsView.bottomAnchor constraintEqualToAnchor:self.bottomAnchor],
      [self.controlsView.heightAnchor constraintEqualToConstant:44.0],
    ]];
  }
  return self;
}

@end

@interface ErikaMetalDemoDelegate : NSObject <NSApplicationDelegate>
@property(nonatomic, strong) NSWindow *window;
@property(nonatomic, strong) NSTimer *smokeTimer;
@end

@implementation ErikaMetalDemoDelegate

- (void)applicationDidFinishLaunching:(NSNotification *)notification {
  (void)notification;
  NSRect frame = NSMakeRect(0, 0, 960, 540);
  self.window = [[NSWindow alloc] initWithContentRect:frame
                                            styleMask:(NSWindowStyleMaskTitled |
                                                       NSWindowStyleMaskClosable |
                                                       NSWindowStyleMaskMiniaturizable |
                                                       NSWindowStyleMaskResizable)
                                              backing:NSBackingStoreBuffered
                                                defer:NO];
  self.window.title = @"Erika Metal Demo";
  self.window.contentView = [[ErikaPlayerContainerView alloc] initWithFrame:frame];
  [self.window center];
  [self.window makeKeyAndOrderFront:nil];
  double smokeSeconds = erika_demo_smoke_seconds();
  // Always activate: an inactive app gets App-Napped by macOS, which
  // throttles the render timer and stalls the presentation-driven decode.
  [NSApp activateIgnoringOtherApps:YES];
  if (smokeSeconds > 0.0) {
    self.smokeTimer = [NSTimer scheduledTimerWithTimeInterval:smokeSeconds
                                                       target:self
                                                     selector:@selector(smokeTimerFired:)
                                                     userInfo:nil
                                                      repeats:NO];
    [[NSRunLoop mainRunLoop] addTimer:self.smokeTimer forMode:NSRunLoopCommonModes];
  }
}

- (void)smokeTimerFired:(NSTimer *)timer {
  (void)timer;
  [NSApp terminate:nil];
}

- (BOOL)applicationShouldTerminateAfterLastWindowClosed:(NSApplication *)sender {
  (void)sender;
  return YES;
}

@end

void erika_demo_run_app(void) {
  @autoreleasepool {
    NSApplication *app = [NSApplication sharedApplication];
    app.activationPolicy = NSApplicationActivationPolicyRegular;
    ErikaMetalDemoDelegate *delegate = [[ErikaMetalDemoDelegate alloc] init];
    app.delegate = delegate;
    [app run];
  }
}
