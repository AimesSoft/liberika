#import <AppKit/AppKit.h>
#import <QuartzCore/QuartzCore.h>
#import <math.h>

extern void erika_perf_lab_attach_layer(void *layer, unsigned int width, unsigned int height, double scale);
extern void erika_perf_lab_resize_layer(unsigned int width, unsigned int height, double scale);
extern void erika_perf_lab_render_frame(double host_time_seconds);
extern bool erika_perf_lab_should_auto_exit(void);
extern void erika_perf_lab_toggle_play_pause(void);
extern void erika_perf_lab_seek_seconds(double seconds);
extern double erika_perf_lab_position_seconds(void);
extern double erika_perf_lab_duration_seconds(void);
extern bool erika_perf_lab_is_playing(void);
extern const char *erika_perf_lab_metrics_text(void);
extern void erika_perf_lab_set_density(double comments_per_second);
extern void erika_perf_lab_set_font_size(double font_size);
extern void erika_perf_lab_set_display_area(double display_area);
extern void erika_perf_lab_set_outline(double outline_width);
extern double erika_perf_lab_density(void);
extern double erika_perf_lab_font_size(void);
extern double erika_perf_lab_display_area(void);
extern double erika_perf_lab_outline(void);
extern double erika_perf_lab_window_width(void);
extern double erika_perf_lab_window_height(void);
extern bool erika_perf_lab_fullscreen(void);
extern bool erika_perf_lab_uncapped(void);
extern double erika_perf_lab_target_fps(void);
extern bool erika_perf_lab_hide_panel(void);
extern double erika_perf_lab_surface_scale_override(void);
extern void erika_perf_lab_run_app(void);

static NSString *ErikaLabFormatTime(double seconds) {
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

@interface ErikaPerfLabVideoView : NSView
@property(nonatomic, strong) CAMetalLayer *metalLayer;
@property(nonatomic, strong) NSTimer *timer;
@property(nonatomic, assign) CFTimeInterval startTime;
@property(nonatomic, assign) BOOL uncappedPumpActive;
@end

@implementation ErikaPerfLabVideoView

- (instancetype)initWithFrame:(NSRect)frameRect {
  self = [super initWithFrame:frameRect];
  if (self) {
    self.wantsLayer = YES;
    self.metalLayer = [CAMetalLayer layer];
    self.metalLayer.pixelFormat = MTLPixelFormatBGRA8Unorm;
    self.metalLayer.framebufferOnly = YES;
    self.metalLayer.opaque = YES;
    if ([self.metalLayer respondsToSelector:@selector(setDisplaySyncEnabled:)]) {
      self.metalLayer.displaySyncEnabled = !erika_perf_lab_uncapped();
    }
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
    if (erika_perf_lab_uncapped()) {
      self.uncappedPumpActive = YES;
      [self scheduleUncappedRenderTick];
    } else {
      double targetFps = erika_perf_lab_target_fps();
      if (!isfinite(targetFps) || targetFps <= 0.0) {
        targetFps = 60.0;
      }
      self.timer = [NSTimer scheduledTimerWithTimeInterval:(1.0 / targetFps)
                                                    target:self
                                                  selector:@selector(renderTick:)
                                                  userInfo:nil
                                                   repeats:YES];
      [[NSRunLoop mainRunLoop] addTimer:self.timer forMode:NSRunLoopCommonModes];
    }
  }
}

- (void)viewWillMoveToWindow:(NSWindow *)newWindow {
  if (newWindow == nil) {
    self.uncappedPumpActive = NO;
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
  double scaleOverride = erika_perf_lab_surface_scale_override();
  if (isfinite(scaleOverride) && scaleOverride > 0.0) {
    scale = (CGFloat)scaleOverride;
  }
  CGSize drawableSize = CGSizeMake(MAX(1.0, self.bounds.size.width * scale), MAX(1.0, self.bounds.size.height * scale));
  self.metalLayer.drawableSize = drawableSize;
  self.metalLayer.frame = self.bounds;
  unsigned int pixelWidth = (unsigned int)MAX(1.0, round(drawableSize.width));
  unsigned int pixelHeight = (unsigned int)MAX(1.0, round(drawableSize.height));
  if (attach) {
    erika_perf_lab_attach_layer((__bridge void *)self.metalLayer, pixelWidth, pixelHeight, scale);
  } else {
    erika_perf_lab_resize_layer(pixelWidth, pixelHeight, scale);
  }
}

- (void)renderTick:(NSTimer *)timer {
  (void)timer;
  erika_perf_lab_render_frame(CACurrentMediaTime() - self.startTime);
  if (erika_perf_lab_should_auto_exit()) {
    [NSApp terminate:nil];
  }
}

- (void)scheduleUncappedRenderTick {
  __weak ErikaPerfLabVideoView *weakSelf = self;
  dispatch_async(dispatch_get_main_queue(), ^{
    ErikaPerfLabVideoView *strongSelf = weakSelf;
    if (strongSelf == nil || !strongSelf.uncappedPumpActive || strongSelf.window == nil) {
      return;
    }
    erika_perf_lab_render_frame(CACurrentMediaTime() - strongSelf.startTime);
    if (erika_perf_lab_should_auto_exit()) {
      [NSApp terminate:nil];
      return;
    }
    [strongSelf scheduleUncappedRenderTick];
  });
}

@end

@interface ErikaPerfLabSliderRow : NSView
@property(nonatomic, strong) NSTextField *titleLabel;
@property(nonatomic, strong) NSSlider *slider;
@property(nonatomic, strong) NSTextField *valueLabel;
@end

@implementation ErikaPerfLabSliderRow

- (instancetype)initWithTitle:(NSString *)title min:(double)min max:(double)max value:(double)value target:(id)target action:(SEL)action {
  self = [super initWithFrame:NSZeroRect];
  if (self) {
    self.translatesAutoresizingMaskIntoConstraints = NO;
    self.titleLabel = [NSTextField labelWithString:title];
    self.titleLabel.textColor = [NSColor colorWithWhite:0.78 alpha:1.0];
    self.titleLabel.font = [NSFont systemFontOfSize:12.0 weight:NSFontWeightMedium];
    self.titleLabel.translatesAutoresizingMaskIntoConstraints = NO;
    [self addSubview:self.titleLabel];

    self.slider = [[NSSlider alloc] initWithFrame:NSZeroRect];
    self.slider.minValue = min;
    self.slider.maxValue = max;
    self.slider.doubleValue = value;
    self.slider.continuous = YES;
    self.slider.target = target;
    self.slider.action = action;
    self.slider.translatesAutoresizingMaskIntoConstraints = NO;
    [self addSubview:self.slider];

    self.valueLabel = [NSTextField labelWithString:@""];
    self.valueLabel.textColor = NSColor.whiteColor;
    self.valueLabel.alignment = NSTextAlignmentRight;
    self.valueLabel.font = [NSFont monospacedDigitSystemFontOfSize:12.0 weight:NSFontWeightRegular];
    self.valueLabel.translatesAutoresizingMaskIntoConstraints = NO;
    [self addSubview:self.valueLabel];

    [NSLayoutConstraint activateConstraints:@[
      [self.titleLabel.leadingAnchor constraintEqualToAnchor:self.leadingAnchor],
      [self.titleLabel.topAnchor constraintEqualToAnchor:self.topAnchor],
      [self.titleLabel.widthAnchor constraintEqualToConstant:112.0],
      [self.slider.leadingAnchor constraintEqualToAnchor:self.titleLabel.trailingAnchor constant:8.0],
      [self.slider.trailingAnchor constraintEqualToAnchor:self.valueLabel.leadingAnchor constant:-8.0],
      [self.slider.centerYAnchor constraintEqualToAnchor:self.titleLabel.centerYAnchor],
      [self.valueLabel.trailingAnchor constraintEqualToAnchor:self.trailingAnchor],
      [self.valueLabel.centerYAnchor constraintEqualToAnchor:self.titleLabel.centerYAnchor],
      [self.valueLabel.widthAnchor constraintEqualToConstant:72.0],
      [self.heightAnchor constraintEqualToConstant:28.0],
    ]];
  }
  return self;
}

@end

@interface ErikaPerfLabPanel : NSView
@property(nonatomic, strong) NSScrollView *metricsScrollView;
@property(nonatomic, strong) NSTextView *metricsTextView;
@property(nonatomic, strong) ErikaPerfLabSliderRow *densityRow;
@property(nonatomic, strong) ErikaPerfLabSliderRow *fontRow;
@property(nonatomic, strong) ErikaPerfLabSliderRow *areaRow;
@property(nonatomic, strong) ErikaPerfLabSliderRow *outlineRow;
@property(nonatomic, strong) NSTimer *timer;
@end

@implementation ErikaPerfLabPanel

- (instancetype)initWithFrame:(NSRect)frameRect {
  self = [super initWithFrame:frameRect];
  if (self) {
    self.wantsLayer = YES;
    self.layer.backgroundColor = [NSColor colorWithWhite:0.075 alpha:1.0].CGColor;

    NSTextField *title = [NSTextField labelWithString:@"Danmaku Perf Lab"];
    title.textColor = NSColor.whiteColor;
    title.font = [NSFont systemFontOfSize:18.0 weight:NSFontWeightSemibold];
    title.translatesAutoresizingMaskIntoConstraints = NO;
    [self addSubview:title];

    self.densityRow = [[ErikaPerfLabSliderRow alloc] initWithTitle:@"Density / s" min:1.0 max:600.0 value:erika_perf_lab_density() target:self action:@selector(densityChanged:)];
    self.fontRow = [[ErikaPerfLabSliderRow alloc] initWithTitle:@"Font size" min:12.0 max:72.0 value:erika_perf_lab_font_size() target:self action:@selector(fontChanged:)];
    self.areaRow = [[ErikaPerfLabSliderRow alloc] initWithTitle:@"Display area" min:0.25 max:1.0 value:erika_perf_lab_display_area() target:self action:@selector(areaChanged:)];
    self.outlineRow = [[ErikaPerfLabSliderRow alloc] initWithTitle:@"Outline" min:0.0 max:5.0 value:erika_perf_lab_outline() target:self action:@selector(outlineChanged:)];
    [self addSubview:self.densityRow];
    [self addSubview:self.fontRow];
    [self addSubview:self.areaRow];
    [self addSubview:self.outlineRow];

    self.metricsScrollView = [[NSScrollView alloc] initWithFrame:NSZeroRect];
    self.metricsScrollView.hasVerticalScroller = YES;
    self.metricsScrollView.hasHorizontalScroller = NO;
    self.metricsScrollView.borderType = NSNoBorder;
    self.metricsScrollView.drawsBackground = NO;
    self.metricsScrollView.translatesAutoresizingMaskIntoConstraints = NO;

    self.metricsTextView = [[NSTextView alloc] initWithFrame:NSZeroRect];
    self.metricsTextView.editable = NO;
    self.metricsTextView.selectable = YES;
    self.metricsTextView.drawsBackground = NO;
    self.metricsTextView.textColor = [NSColor colorWithWhite:0.9 alpha:1.0];
    self.metricsTextView.font = [NSFont monospacedDigitSystemFontOfSize:12.0 weight:NSFontWeightRegular];
    self.metricsTextView.textContainerInset = NSMakeSize(0.0, 0.0);
    self.metricsTextView.textContainer.lineFragmentPadding = 0.0;
    self.metricsTextView.textContainer.widthTracksTextView = YES;
    self.metricsTextView.horizontallyResizable = NO;
    self.metricsTextView.verticallyResizable = YES;
    self.metricsTextView.autoresizingMask = NSViewWidthSizable;
    self.metricsScrollView.documentView = self.metricsTextView;
    [self addSubview:self.metricsScrollView];

    [NSLayoutConstraint activateConstraints:@[
      [title.leadingAnchor constraintEqualToAnchor:self.leadingAnchor constant:16.0],
      [title.trailingAnchor constraintEqualToAnchor:self.trailingAnchor constant:-16.0],
      [title.topAnchor constraintEqualToAnchor:self.topAnchor constant:16.0],
      [self.densityRow.leadingAnchor constraintEqualToAnchor:self.leadingAnchor constant:16.0],
      [self.densityRow.trailingAnchor constraintEqualToAnchor:self.trailingAnchor constant:-16.0],
      [self.densityRow.topAnchor constraintEqualToAnchor:title.bottomAnchor constant:16.0],
      [self.fontRow.leadingAnchor constraintEqualToAnchor:self.densityRow.leadingAnchor],
      [self.fontRow.trailingAnchor constraintEqualToAnchor:self.densityRow.trailingAnchor],
      [self.fontRow.topAnchor constraintEqualToAnchor:self.densityRow.bottomAnchor constant:8.0],
      [self.areaRow.leadingAnchor constraintEqualToAnchor:self.densityRow.leadingAnchor],
      [self.areaRow.trailingAnchor constraintEqualToAnchor:self.densityRow.trailingAnchor],
      [self.areaRow.topAnchor constraintEqualToAnchor:self.fontRow.bottomAnchor constant:8.0],
      [self.outlineRow.leadingAnchor constraintEqualToAnchor:self.densityRow.leadingAnchor],
      [self.outlineRow.trailingAnchor constraintEqualToAnchor:self.densityRow.trailingAnchor],
      [self.outlineRow.topAnchor constraintEqualToAnchor:self.areaRow.bottomAnchor constant:8.0],
      [self.metricsScrollView.leadingAnchor constraintEqualToAnchor:self.leadingAnchor constant:16.0],
      [self.metricsScrollView.trailingAnchor constraintEqualToAnchor:self.trailingAnchor constant:-16.0],
      [self.metricsScrollView.topAnchor constraintEqualToAnchor:self.outlineRow.bottomAnchor constant:18.0],
      [self.metricsScrollView.bottomAnchor constraintEqualToAnchor:self.bottomAnchor constant:-16.0],
    ]];
    [self refreshPanel:nil];
  }
  return self;
}

- (void)viewDidMoveToWindow {
  [super viewDidMoveToWindow];
  if (self.window != nil && self.timer == nil) {
    self.timer = [NSTimer scheduledTimerWithTimeInterval:0.25
                                                  target:self
                                                selector:@selector(refreshPanel:)
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

- (void)densityChanged:(NSSlider *)sender {
  erika_perf_lab_set_density(sender.doubleValue);
  [self refreshPanel:nil];
}

- (void)fontChanged:(NSSlider *)sender {
  erika_perf_lab_set_font_size(sender.doubleValue);
  [self refreshPanel:nil];
}

- (void)areaChanged:(NSSlider *)sender {
  erika_perf_lab_set_display_area(sender.doubleValue);
  [self refreshPanel:nil];
}

- (void)outlineChanged:(NSSlider *)sender {
  erika_perf_lab_set_outline(sender.doubleValue);
  [self refreshPanel:nil];
}

- (void)refreshPanel:(NSTimer *)timer {
  (void)timer;
  self.densityRow.valueLabel.stringValue = [NSString stringWithFormat:@"%.0f", erika_perf_lab_density()];
  self.fontRow.valueLabel.stringValue = [NSString stringWithFormat:@"%.1f", erika_perf_lab_font_size()];
  self.areaRow.valueLabel.stringValue = [NSString stringWithFormat:@"%.2f", erika_perf_lab_display_area()];
  self.outlineRow.valueLabel.stringValue = [NSString stringWithFormat:@"%.1f", erika_perf_lab_outline()];
  const char *metrics = erika_perf_lab_metrics_text();
  self.metricsTextView.string = metrics != NULL ? [NSString stringWithUTF8String:metrics] : @"";
}

@end

@interface ErikaPerfLabControls : NSView
@property(nonatomic, strong) NSButton *playPauseButton;
@property(nonatomic, strong) NSSlider *progressSlider;
@property(nonatomic, strong) NSTextField *timeLabel;
@property(nonatomic, strong) NSTimer *timer;
@property(nonatomic, assign) BOOL scrubbing;
@end

@implementation ErikaPerfLabControls

- (instancetype)initWithFrame:(NSRect)frameRect {
  self = [super initWithFrame:frameRect];
  if (self) {
    self.wantsLayer = YES;
    self.layer.backgroundColor = [NSColor colorWithWhite:0.06 alpha:1.0].CGColor;

    self.playPauseButton = [NSButton buttonWithTitle:@"Pause" target:self action:@selector(togglePlayPause:)];
    self.playPauseButton.bezelStyle = NSBezelStyleRegularSquare;
    self.playPauseButton.bordered = NO;
    self.playPauseButton.wantsLayer = YES;
    self.playPauseButton.layer.backgroundColor = [NSColor colorWithWhite:0.22 alpha:1.0].CGColor;
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
    self.timeLabel.font = [NSFont monospacedDigitSystemFontOfSize:13.0 weight:NSFontWeightRegular];
    self.timeLabel.translatesAutoresizingMaskIntoConstraints = NO;
    [self addSubview:self.timeLabel];

    [NSLayoutConstraint activateConstraints:@[
      [self.playPauseButton.leadingAnchor constraintEqualToAnchor:self.leadingAnchor constant:12.0],
      [self.playPauseButton.centerYAnchor constraintEqualToAnchor:self.centerYAnchor],
      [self.playPauseButton.widthAnchor constraintEqualToConstant:84.0],
      [self.progressSlider.leadingAnchor constraintEqualToAnchor:self.playPauseButton.trailingAnchor constant:12.0],
      [self.progressSlider.trailingAnchor constraintEqualToAnchor:self.timeLabel.leadingAnchor constant:-12.0],
      [self.progressSlider.centerYAnchor constraintEqualToAnchor:self.centerYAnchor],
      [self.timeLabel.trailingAnchor constraintEqualToAnchor:self.trailingAnchor constant:-12.0],
      [self.timeLabel.centerYAnchor constraintEqualToAnchor:self.centerYAnchor],
      [self.timeLabel.widthAnchor constraintEqualToConstant:132.0],
    ]];
  }
  return self;
}

- (void)viewDidMoveToWindow {
  [super viewDidMoveToWindow];
  if (self.window != nil && self.timer == nil) {
    self.timer = [NSTimer scheduledTimerWithTimeInterval:0.25 target:self selector:@selector(refreshControls:) userInfo:nil repeats:YES];
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
  erika_perf_lab_toggle_play_pause();
  [self refreshControls:nil];
}

- (void)sliderChanged:(NSSlider *)sender {
  double duration = erika_perf_lab_duration_seconds();
  if (duration <= 0.0 || !isfinite(duration)) {
    return;
  }
  self.scrubbing = YES;
  erika_perf_lab_seek_seconds(sender.doubleValue);
  [self refreshControls:nil];
  self.scrubbing = NO;
}

- (void)refreshControls:(NSTimer *)timer {
  (void)timer;
  double duration = erika_perf_lab_duration_seconds();
  double position = erika_perf_lab_position_seconds();
  BOOL hasDuration = duration > 0.0 && isfinite(duration);
  self.progressSlider.enabled = hasDuration;
  self.progressSlider.maxValue = hasDuration ? duration : 1.0;
  if (!self.scrubbing) {
    self.progressSlider.doubleValue = hasDuration ? MIN(MAX(position, 0.0), duration) : 0.0;
  }
  self.playPauseButton.title = erika_perf_lab_is_playing() ? @"Pause" : @"Play";
  self.timeLabel.stringValue = [NSString stringWithFormat:@"%@ / %@", ErikaLabFormatTime(position), ErikaLabFormatTime(duration)];
}

@end

@interface ErikaPerfLabRootView : NSView
@property(nonatomic, strong) ErikaPerfLabVideoView *videoView;
@property(nonatomic, strong) ErikaPerfLabPanel *panelView;
@property(nonatomic, strong) ErikaPerfLabControls *controlsView;
@end

@implementation ErikaPerfLabRootView

- (instancetype)initWithFrame:(NSRect)frameRect {
  self = [super initWithFrame:frameRect];
  if (self) {
    self.wantsLayer = YES;
    self.layer.backgroundColor = NSColor.blackColor.CGColor;

    self.videoView = [[ErikaPerfLabVideoView alloc] initWithFrame:NSZeroRect];
    self.videoView.translatesAutoresizingMaskIntoConstraints = NO;
    [self addSubview:self.videoView];

    self.panelView = [[ErikaPerfLabPanel alloc] initWithFrame:NSZeroRect];
    self.panelView.translatesAutoresizingMaskIntoConstraints = NO;
    [self addSubview:self.panelView];

    self.controlsView = [[ErikaPerfLabControls alloc] initWithFrame:NSZeroRect];
    self.controlsView.translatesAutoresizingMaskIntoConstraints = NO;
    [self addSubview:self.controlsView];

    BOOL hidePanel = erika_perf_lab_hide_panel();
    [NSLayoutConstraint activateConstraints:@[
      [self.videoView.leadingAnchor constraintEqualToAnchor:self.leadingAnchor],
      [self.videoView.topAnchor constraintEqualToAnchor:self.topAnchor],
      [self.videoView.bottomAnchor constraintEqualToAnchor:self.controlsView.topAnchor],
      [self.videoView.trailingAnchor constraintEqualToAnchor:hidePanel ? self.trailingAnchor : self.panelView.leadingAnchor],
      [self.panelView.topAnchor constraintEqualToAnchor:self.topAnchor],
      [self.panelView.trailingAnchor constraintEqualToAnchor:self.trailingAnchor],
      [self.panelView.bottomAnchor constraintEqualToAnchor:self.bottomAnchor],
      [self.panelView.widthAnchor constraintEqualToConstant:hidePanel ? 0.0 : 320.0],
      [self.controlsView.leadingAnchor constraintEqualToAnchor:self.leadingAnchor],
      [self.controlsView.trailingAnchor constraintEqualToAnchor:self.videoView.trailingAnchor],
      [self.controlsView.bottomAnchor constraintEqualToAnchor:self.bottomAnchor],
      [self.controlsView.heightAnchor constraintEqualToConstant:46.0],
    ]];
    self.panelView.hidden = hidePanel;
  }
  return self;
}

@end

@interface ErikaPerfLabDelegate : NSObject <NSApplicationDelegate>
@property(nonatomic, strong) NSWindow *window;
@end

@implementation ErikaPerfLabDelegate

- (void)applicationDidFinishLaunching:(NSNotification *)notification {
  (void)notification;
  NSRect visibleFrame = NSScreen.mainScreen.visibleFrame;
  BOOL fullscreen = erika_perf_lab_fullscreen();
  double requestedWidth = erika_perf_lab_window_width();
  double requestedHeight = erika_perf_lab_window_height();
  CGFloat width = fullscreen ? visibleFrame.size.width : (requestedWidth > 0.0 ? MIN((CGFloat)requestedWidth, visibleFrame.size.width * 0.96) : MIN(1280.0, visibleFrame.size.width * 0.90));
  CGFloat height = fullscreen ? visibleFrame.size.height : (requestedHeight > 0.0 ? MIN((CGFloat)requestedHeight, visibleFrame.size.height * 0.90) : MIN(720.0, visibleFrame.size.height * 0.78));
  NSRect frame = fullscreen ? visibleFrame : NSMakeRect(NSMidX(visibleFrame) - width * 0.5,
                                                        NSMidY(visibleFrame) - height * 0.5,
                                                        width,
                                                        height);
  self.window = [[NSWindow alloc] initWithContentRect:frame
                                            styleMask:(NSWindowStyleMaskTitled |
                                                       NSWindowStyleMaskClosable |
                                                       NSWindowStyleMaskMiniaturizable |
                                                       NSWindowStyleMaskResizable)
                                              backing:NSBackingStoreBuffered
                                                defer:NO];
  self.window.title = @"Erika Danmaku Perf Lab";
  self.window.contentView = [[ErikaPerfLabRootView alloc] initWithFrame:NSMakeRect(0, 0, width, height)];
  [self.window makeKeyAndOrderFront:nil];
  [NSApp activateIgnoringOtherApps:YES];
}

- (BOOL)applicationShouldTerminateAfterLastWindowClosed:(NSApplication *)sender {
  (void)sender;
  return YES;
}

@end

void erika_perf_lab_run_app(void) {
  @autoreleasepool {
    NSApplication *app = [NSApplication sharedApplication];
    app.activationPolicy = NSApplicationActivationPolicyRegular;
    ErikaPerfLabDelegate *delegate = [[ErikaPerfLabDelegate alloc] init];
    app.delegate = delegate;
    [app run];
  }
}
