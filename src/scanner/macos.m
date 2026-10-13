// Image Capture drivers run outside the camera worker. This helper opens one
// session, transfers one document and exits; it never creates an application UI.
#import <Foundation/Foundation.h>
#import <ImageCaptureCore/ImageCaptureCore.h>
#import <ImageIO/ImageIO.h>
#import <CoreGraphics/CoreGraphics.h>
#include <errno.h>
#include <dispatch/dispatch.h>
#include <math.h>
#include <stdio.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <unistd.h>

static const size_t MaximumImageBytes = 256 * 1024 * 1024;
static const size_t MaximumFrameBytes = 128 * 1024 * 1024;
static atomic_bool ParentLost = false;

static dispatch_source_t WatchParent(pid_t originalParent) {
    // Driver calls can hang the main thread. Detect a dead camera worker on an
    // independent queue, with a short grace period for main-loop cleanup.
    dispatch_queue_t queue = dispatch_get_global_queue(QOS_CLASS_UTILITY, 0);
    dispatch_source_t watch = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0, queue);
    dispatch_source_set_timer(watch, dispatch_time(DISPATCH_TIME_NOW, 200 * NSEC_PER_MSEC),
                             200 * NSEC_PER_MSEC, 20 * NSEC_PER_MSEC);
    dispatch_source_set_event_handler(watch, ^{
        if (getppid() != originalParent && !atomic_exchange(&ParentLost, true)) {
            dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 500 * NSEC_PER_MSEC), queue, ^{
                _exit(1);
            });
        }
    });
    dispatch_resume(watch);
    return watch;
}

static void RunUntil(NSDate *deadline) {
    [[NSRunLoop currentRunLoop] runUntilDate:deadline];
}

static NSString *ShortString(NSString *value, NSUInteger limit) {
    if (!value) return @"";
    return value.length <= limit ? value : [value substringToIndex:limit];
}

@interface ScannerClient : NSObject <ICDeviceBrowserDelegate, ICScannerDeviceDelegate>
@property(nonatomic, strong) ICDeviceBrowser *browser;
@property(nonatomic, strong) ICScannerDevice *scanner;
@property(nonatomic, strong) NSMutableDictionary<NSString *, ICScannerDevice *> *devices;
@property(nonatomic, copy) NSString *deviceID;
@property(nonatomic, copy) NSString *mode;
@property(nonatomic, copy) NSString *source;
@property(nonatomic, strong) NSURL *directory;
@property(nonatomic, strong) NSURL *page;
@property(nonatomic, copy) NSString *failure;
@property(nonatomic, strong) NSDate *pageDeadline;
@property(nonatomic) NSUInteger dpi;
@property(nonatomic) BOOL sessionOpened;
@property(nonatomic) BOOL selecting;
@property(nonatomic) BOOL scanStarted;
@property(nonatomic) BOOL done;
- (void)fail:(NSString *)message;
- (void)configureWhenReady;
@end

@implementation ScannerClient

- (instancetype)init {
    self = [super init];
    if (self) {
        _devices = [NSMutableDictionary dictionary];
        _browser = [[ICDeviceBrowser alloc] init];
        _browser.delegate = self;
        _browser.browsedDeviceTypeMask =
            (ICDeviceTypeMask)(ICDeviceTypeMaskScanner |
                ICDeviceLocationTypeMaskLocal | ICDeviceLocationTypeMaskRemote);
    }
    return self;
}

- (void)fail:(NSString *)message {
    if (self.done) return;
    self.failure = message;
    self.done = YES;
    if (self.scanStarted) [self.scanner cancelScan];
}

- (void)deviceBrowser:(ICDeviceBrowser *)browser
         didAddDevice:(ICDevice *)device moreComing:(BOOL)moreComing {
    (void)browser;
    (void)moreComing;
    if (![device isKindOfClass:[ICScannerDevice class]] ||
        !device.UUIDString.length || device.UUIDString.length > 4096) return;
    ICScannerDevice *scanner = (ICScannerDevice *)device;
    self.devices[scanner.UUIDString] = scanner;
    if (self.deviceID && !self.scanner &&
        [self.deviceID isEqualToString:scanner.UUIDString]) {
        self.scanner = scanner;
        scanner.delegate = self;
        [scanner requestOpenSession];
    }
}

- (void)deviceBrowser:(ICDeviceBrowser *)browser
      didRemoveDevice:(ICDevice *)device moreGoing:(BOOL)moreGoing {
    (void)browser;
    (void)moreGoing;
    if (device.UUIDString) [self.devices removeObjectForKey:device.UUIDString];
    if (device == self.scanner) [self fail:@"scanner unavailable: device disconnected"];
}

- (void)didRemoveDevice:(ICDevice *)device {
    if (device == self.scanner) [self fail:@"scanner unavailable: device disconnected"];
}

- (void)device:(ICDevice *)device didOpenSessionWithError:(NSError *)error {
    (void)device;
    if (error) {
        [self fail:[NSString stringWithFormat:@"scanner unavailable: cannot open session: %@",
                     error.localizedDescription]];
        return;
    }
    self.sessionOpened = YES;
    [self configureWhenReady];
}

- (void)device:(ICDevice *)device didCloseSessionWithError:(NSError *)error {
    (void)device;
    (void)error;
    if (!self.done && !self.page) [self fail:@"scanner unavailable: session closed"];
}

- (void)deviceDidBecomeReady:(ICDevice *)device {
    (void)device;
    [self configureWhenReady];
}

- (void)device:(ICDevice *)device didEncounterError:(NSError *)error {
    (void)device;
    if (!self.page && error)
        [self fail:[NSString stringWithFormat:@"scanner operation failed: %@",
                     error.localizedDescription]];
}

- (void)scannerDevice:(ICScannerDevice *)scanner
 didSelectFunctionalUnit:(ICScannerFunctionalUnit *)unit error:(NSError *)error {
    (void)scanner;
    (void)unit;
    if (!self.sessionOpened) return;
    self.selecting = NO;
    if (error) {
        [self fail:[NSString stringWithFormat:@"scanner source unsupported: %@",
                     error.localizedDescription]];
        return;
    }
    [self configureWhenReady];
}

- (void)configureWhenReady {
    if (self.done || self.scanStarted || !self.sessionOpened || self.selecting) return;
    NSArray<NSNumber *> *available = self.scanner.availableFunctionalUnitTypes;
    if (!available.count) return;
    ICScannerFunctionalUnit *unit = self.scanner.selectedFunctionalUnit;
    ICScannerFunctionalUnitType wanted;
    if ([self.source isEqualToString:@"Feeder"]) {
        wanted = ICScannerFunctionalUnitTypeDocumentFeeder;
    } else if ([self.source isEqualToString:@"Flatbed"]) {
        wanted = ICScannerFunctionalUnitTypeFlatbed;
    } else if (unit &&
               (unit.type == ICScannerFunctionalUnitTypeFlatbed ||
                unit.type == ICScannerFunctionalUnitTypeDocumentFeeder) &&
               [available containsObject:@(unit.type)]) {
        wanted = unit.type;
    } else if ([available containsObject:@(ICScannerFunctionalUnitTypeFlatbed)]) {
        wanted = ICScannerFunctionalUnitTypeFlatbed;
    } else {
        wanted = ICScannerFunctionalUnitTypeDocumentFeeder;
    }
    if (![available containsObject:@(wanted)]) {
        [self fail:[NSString stringWithFormat:@"scanner source unsupported: %@", self.source]];
        return;
    }
    if (!unit || unit.type != wanted) {
        self.selecting = YES;
        [self.scanner requestSelectFunctionalUnit:wanted];
        return;
    }
    if (!(unit.state & ICScannerFunctionalUnitStateReady) ||
        !unit.supportedResolutions.count) return;
    if (![unit.supportedResolutions containsIndex:self.dpi]) {
        [self fail:[NSString stringWithFormat:@"scanner resolution unsupported: %lu DPI",
                     (unsigned long)self.dpi]];
        return;
    }
    unit.resolution = self.dpi;
    if (unit.resolution != self.dpi) {
        [self fail:@"scanner resolution unsupported: driver changed requested DPI"];
        return;
    }
    ICScannerPixelDataType pixelType = [self.mode isEqualToString:@"Color"]
        ? ICScannerPixelDataTypeRGB : [self.mode isEqualToString:@"Lineart"]
        ? ICScannerPixelDataTypeBW : ICScannerPixelDataTypeGray;
    unit.pixelDataType = pixelType;
    ICScannerBitDepth depth = pixelType == ICScannerPixelDataTypeBW
        ? ICScannerBitDepth1Bit : ICScannerBitDepth8Bits;
    if (unit.pixelDataType != pixelType || ![unit.supportedBitDepths containsIndex:depth]) {
        [self fail:[NSString stringWithFormat:@"scanner color mode unsupported: %@", self.mode]];
        return;
    }
    unit.bitDepth = depth;
    if (unit.bitDepth != depth) {
        [self fail:@"scanner bit depth unsupported: driver changed requested bit depth"];
        return;
    }
    if ([unit.supportedMeasurementUnits containsIndex:ICScannerMeasurementUnitInches])
        unit.measurementUnit = ICScannerMeasurementUnitInches;
    if ([unit.supportedScaleFactors containsIndex:100]) unit.scaleFactor = 100;
    ICSize size = unit.physicalSize;
    if (wanted == ICScannerFunctionalUnitTypeDocumentFeeder) {
        ICScannerFunctionalUnitDocumentFeeder *feeder =
            (ICScannerFunctionalUnitDocumentFeeder *)unit;
        feeder.duplexScanningEnabled = NO;
        feeder.oddPageOrientation = ICEXIFOrientation1;
        size = feeder.documentSize;
    }
    double pixelsPerUnit;
    switch (unit.measurementUnit) {
        case ICScannerMeasurementUnitInches: pixelsPerUnit = self.dpi; break;
        case ICScannerMeasurementUnitCentimeters: pixelsPerUnit = self.dpi / 2.54; break;
        case ICScannerMeasurementUnitPicas: pixelsPerUnit = self.dpi / 6.0; break;
        case ICScannerMeasurementUnitPoints: pixelsPerUnit = self.dpi / 72.0; break;
        case ICScannerMeasurementUnitTwips: pixelsPerUnit = self.dpi / 1440.0; break;
        case ICScannerMeasurementUnitPixels: pixelsPerUnit = 1; break;
        default: [self fail:@"scanner measurement unit unsupported"]; return;
    }
    double scale = unit.scaleFactor ? unit.scaleFactor / 100.0 : 1.0;
    double width = ceil(size.width * pixelsPerUnit * scale);
    double height = ceil(size.height * pixelsPerUnit * scale);
    NSUInteger components = pixelType == ICScannerPixelDataTypeRGB ? 4 : 1;
    NSUInteger frameComponents = pixelType == ICScannerPixelDataTypeRGB ? 3 : 1;
    if (!isfinite(width) || !isfinite(height) || width < 1 || height < 1 ||
        width * height > MaximumImageBytes / components ||
        width * height > MaximumFrameBytes / frameComponents) {
        [self fail:@"scanner image exceeds the 128 MiB frame limit or has invalid dimensions; lower ScanResolution"];
        return;
    }
    unit.scanArea = NSMakeRect(0, 0, size.width, size.height);
    unit.scanAreaOrientation = ICEXIFOrientation1;
    self.scanner.transferMode = ICScannerTransferModeFileBased;
    self.scanner.downloadsDirectory = self.directory;
    self.scanner.documentName = @"document";
    self.scanner.documentUTI = @"public.tiff";
    self.scanStarted = YES;
    [self.scanner requestScan];
}

- (void)scannerDevice:(ICScannerDevice *)scanner didScanToURL:(NSURL *)url {
    if (self.page || self.done) return;
    NSString *prefix = [self.directory.path stringByAppendingString:@"/"];
    NSURL *resolved = [url URLByResolvingSymlinksInPath];
    if (!resolved.isFileURL || ![resolved.path hasPrefix:prefix]) {
        [self fail:@"scanner driver returned a document outside its transfer directory"];
        return;
    }
    self.page = resolved;
    // ICA feeders scan the loaded stack. Cancel as soon as the first page is
    // delivered, and only return that page to the capture pipeline.
    self.pageDeadline = [NSDate dateWithTimeIntervalSinceNow:2];
    [scanner cancelScan];
}

- (void)scannerDevice:(ICScannerDevice *)scanner didCompleteScanWithError:(NSError *)error {
    (void)scanner;
    if (self.done) return;
    if (self.page) {
        self.done = YES; // Cancellation after the first page is successful.
    } else if (error) {
        [self fail:[NSString stringWithFormat:@"scanner operation failed: %@",
                     error.localizedDescription]];
    } else {
        [self fail:@"scanner operation failed: no document was returned"];
    }
}
@end

static BOOL WritePage(NSURL *url, BOOL color, BOOL lineart) {
    NSError *error = nil;
    NSDictionary *attributes = [[NSFileManager defaultManager]
        attributesOfItemAtPath:url.path error:&error];
    if (!attributes || [attributes[NSFileSize] unsignedLongLongValue] > MaximumImageBytes ||
        ![attributes[NSFileType] isEqualToString:NSFileTypeRegular]) {
        fprintf(stderr, "scanner image missing, invalid or exceeds the 256 MiB limit\n");
        return NO;
    }
    CGImageSourceRef source = CGImageSourceCreateWithURL((__bridge CFURLRef)url, NULL);
    if (!source) {
        fprintf(stderr, "scanner operation failed: cannot decode document\n");
        return NO;
    }
    NSDictionary *properties = CFBridgingRelease(CGImageSourceCopyPropertiesAtIndex(source, 0, NULL));
    size_t width = [properties[(__bridge NSString *)kCGImagePropertyPixelWidth] unsignedLongLongValue];
    size_t height = [properties[(__bridge NSString *)kCGImagePropertyPixelHeight] unsignedLongLongValue];
    size_t channels = color ? 4 : 1;
    size_t frameChannels = color ? 3 : 1;
    if (!width || !height || width > MaximumImageBytes / channels ||
        height > MaximumImageBytes / (width * channels) ||
        width > MaximumFrameBytes / frameChannels ||
        height > MaximumFrameBytes / (width * frameChannels)) {
        CFRelease(source);
        fprintf(stderr, "scanner image exceeds the 128 MiB frame limit or has invalid dimensions; lower ScanResolution\n");
        return NO;
    }
    CGImageRef image = CGImageSourceCreateImageAtIndex(source, 0,
        (__bridge CFDictionaryRef)@{(__bridge NSString *)kCGImageSourceShouldCache: @NO});
    CFRelease(source);
    if (!image || CGImageGetWidth(image) != width || CGImageGetHeight(image) != height) {
        if (image) CGImageRelease(image);
        fprintf(stderr, "scanner operation failed: cannot decode document pixels\n");
        return NO;
    }
    size_t rowBytes = width * channels;
    unsigned char *pixels = calloc(height, rowBytes);
    CGColorSpaceRef space = color ? CGColorSpaceCreateWithName(kCGColorSpaceSRGB)
                                 : CGColorSpaceCreateDeviceGray();
    CGBitmapInfo bitmap = color ? (kCGImageAlphaNoneSkipLast | kCGBitmapByteOrder32Big)
                               : (CGBitmapInfo)kCGImageAlphaNone;
    CGContextRef context = pixels && space
        ? CGBitmapContextCreate(pixels, width, height, 8, rowBytes, space, bitmap) : NULL;
    if (space) CGColorSpaceRelease(space);
    if (!context) {
        CGImageRelease(image);
        free(pixels);
        fprintf(stderr, "scanner operation failed: cannot allocate document pixels\n");
        return NO;
    }
    CGContextSetGrayFillColor(context, 1, 1);
    CGContextFillRect(context, CGRectMake(0, 0, width, height));
    CGContextDrawImage(context, CGRectMake(0, 0, width, height), image);
    CGContextRelease(context);
    CGImageRelease(image);
    // Allocate at most one additional RGB row, rather than another full image.
    unsigned char *rgb = color ? malloc(width * 3) : NULL;
    BOOL ok = !color || rgb;
    if (ok) ok = fprintf(stdout, "%s\n%zu %zu\n255\n", color ? "P6" : "P5", width, height) > 0;
    for (size_t y = 0; ok && y < height; ++y) {
        unsigned char *row = pixels + y * rowBytes;
        if (color) {
            for (size_t x = 0; x < width; ++x) {
                rgb[x * 3] = row[x * 4];
                rgb[x * 3 + 1] = row[x * 4 + 1];
                rgb[x * 3 + 2] = row[x * 4 + 2];
            }
            ok = fwrite(rgb, 3, width, stdout) == width;
        } else {
            if (lineart) for (size_t x = 0; x < width; ++x) row[x] = row[x] >= 128 ? 255 : 0;
            ok = fwrite(row, 1, width, stdout) == width;
        }
    }
    free(rgb);
    free(pixels);
    if (!ok || fflush(stdout)) {
        fprintf(stderr, "scanner operation failed: document output pipe closed\n");
        return NO;
    }
    return YES;
}

static double DiscoverySeconds(void) {
    const char *value = getenv("CAPTUREFAB_SCANNER_DISCOVERY_MS");
    if (!value) return 0.6;
    char *end = NULL;
    unsigned long milliseconds = strtoul(value, &end, 10);
    if (end == value || *end) return 0.6;
    return fmax(0.025, fmin(milliseconds, 30000) / 1000.0);
}

static NSDate *DiscoveryDeadline(void) {
    const char *value = getenv("CAPTUREFAB_SCANNER_DISCOVERY_DEADLINE_MS");
    if (value && value[0] >= '0' && value[0] <= '9') {
        char *end = NULL;
        errno = 0;
        unsigned long long milliseconds = strtoull(value, &end, 10);
        if (!errno && end != value && !*end && milliseconds) {
            NSDate *deadline = [NSDate dateWithTimeIntervalSince1970:milliseconds / 1000.0];
            return [deadline earlierDate:[NSDate dateWithTimeIntervalSinceNow:30]];
        }
    }
    return [NSDate dateWithTimeIntervalSinceNow:DiscoverySeconds()];
}

int main(int argc, const char *argv[]) {
    pid_t originalParent = getppid();
    @autoreleasepool {
        ScannerClient *client = nil;
        NSURL *managedDirectory = nil;
        dispatch_source_t parentWatch = nil;
        @try {
            BOOL discover = argc == 2 && strcmp(argv[1], "discover") == 0;
            BOOL scan = argc == 6 && strcmp(argv[1], "scan") == 0;
            if (!discover && !scan) {
                fprintf(stderr, "usage: scanner discover | scan ID DPI Color|Gray|Lineart Default|Flatbed|Feeder\n");
                return 2;
            }
            // The parent's absolute deadline includes framework loading and
            // browser startup, so a short GUI discovery still returns on time.
            NSDate *deadline = discover ? DiscoveryDeadline() : [NSDate dateWithTimeIntervalSinceNow:180];
            client = [[ScannerClient alloc] init];
            if (scan) {
                client.deviceID = [NSString stringWithUTF8String:argv[2]];
                client.mode = [NSString stringWithUTF8String:argv[4]];
                client.source = [NSString stringWithUTF8String:argv[5]];
                char *end = NULL;
                errno = 0;
                unsigned long dpi = strtoul(argv[3], &end, 10);
                if (!client.deviceID.length || client.deviceID.length > 4096 || errno ||
                    end == argv[3] || *end || dpi < 1 || dpi > 9600 ||
                    ![@[@"Color", @"Gray", @"Lineart"] containsObject:client.mode ?: @""] ||
                    ![@[@"Default", @"Flatbed", @"Feeder"] containsObject:client.source ?: @""]) {
                    fprintf(stderr, "scanner usage: invalid device, DPI, mode or source\n");
                    return 2;
                }
                client.dpi = dpi;
                // The worker owns this scratch parent so it can remove driver
                // output even when it has to kill a hung helper. Direct helper
                // invocations fall back to the OS temporary directory.
                const char *transfer = getenv("CAPTUREFAB_SCANNER_TRANSFER_DIR");
                NSString *parent = transfer ? [NSString stringWithUTF8String:transfer] : NSTemporaryDirectory();
                if (transfer && parent)
                    managedDirectory = [[NSURL fileURLWithPath:parent isDirectory:YES] URLByResolvingSymlinksInPath];
                NSString *template = [parent stringByAppendingPathComponent:@"capturefab-scan-XXXXXX"];
                if (!template) {
                    fprintf(stderr, "scanner usage: invalid transfer directory\n");
                    return 2;
                }
                char *path = strdup(template.fileSystemRepresentation);
                if (!path || !mkdtemp(path)) {
                    free(path);
                    fprintf(stderr, "scanner operation failed: cannot create transfer directory\n");
                    return 1;
                }
                client.directory = [[NSURL fileURLWithFileSystemRepresentation:path isDirectory:YES
                    relativeToURL:nil] URLByResolvingSymlinksInPath];
                free(path);
            }
            parentWatch = WatchParent(originalParent);
            if (!discover || deadline.timeIntervalSinceNow > 0) [client.browser start];
            NSDate *lookupDeadline = [NSDate dateWithTimeIntervalSinceNow:10];
            while (!client.done && deadline.timeIntervalSinceNow > 0) {
                RunUntil([NSDate dateWithTimeIntervalSinceNow:0.025]);
                if (atomic_load(&ParentLost)) {
                    [client fail:@"scanner acquisition interrupted: camera worker exited"];
                    break;
                }
                if (scan) {
                    if (!client.scanner && lookupDeadline.timeIntervalSinceNow <= 0)
                        [client fail:@"scanner unavailable: selected device was not found"];
                    [client configureWhenReady];
                    if (client.page && client.pageDeadline.timeIntervalSinceNow <= 0) client.done = YES;
                }
            }
            int status = 0;
            if (atomic_load(&ParentLost)) {
                if (client.scanner.hasOpenSession) [client.scanner requestCloseSession];
                return 1;
            } else if (discover) {
                NSMutableArray *devices = [NSMutableArray array];
                for (NSString *key in [[client.devices allKeys] sortedArrayUsingSelector:@selector(compare:)]) {
                    ICScannerDevice *device = client.devices[key];
                    if (devices.count == 256) break;
                    [devices addObject:@{@"id": key, @"name": ShortString(device.name, 1024),
                        @"vendor": @"", @"serial": ShortString(device.serialNumberString, 1024)}];
                }
                NSData *json = [NSJSONSerialization dataWithJSONObject:devices options:0 error:nil];
                if (!json || fwrite(json.bytes, 1, json.length, stdout) != json.length || fflush(stdout)) status = 1;
                // Discovery never opens a device session or creates transfer
                // files. Closing browser IPC during Objective-C teardown adds
                // hundreds of milliseconds; process exit releases it directly.
                _exit(status);
            } else {
                if (!client.done) [client fail:@"scanner timeout: document scan did not complete"];
                if (client.scanner.hasOpenSession) [client.scanner requestCloseSession];
                RunUntil([NSDate dateWithTimeIntervalSinceNow:0.1]);
                if (client.failure) {
                    fprintf(stderr, "%s\n", client.failure.UTF8String);
                    status = 1;
                } else if (!WritePage(client.page, [client.mode isEqualToString:@"Color"],
                                      [client.mode isEqualToString:@"Lineart"])) {
                    status = 1;
                }
            }
            client.scanner.delegate = nil;
            [client.browser stop];
            return status;
        } @catch (NSException *exception) {
            fprintf(stderr, "scanner operation failed: %s\n", exception.reason.UTF8String);
            return 1;
        } @finally {
            if (client.directory)
                [[NSFileManager defaultManager] removeItemAtURL:client.directory error:nil];
            if (atomic_load(&ParentLost) && managedDirectory)
                (void)rmdir(managedDirectory.fileSystemRepresentation);
            if (parentWatch) dispatch_source_cancel(parentWatch);
        }
    }
}
