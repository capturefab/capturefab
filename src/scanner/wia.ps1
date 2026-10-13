# Embedded Windows helper. Rust passes all settings as environment data and
# invokes this script with -EncodedCommand. Stdout contains JSON or one PNM;
# diagnostics go only to stderr. No WIA CommonDialog is used.
# WIA API/property definitions:
# https://learn.microsoft.com/en-us/previous-versions/windows/desktop/wiaaut/-wiaaut-iitem-transfer
# https://learn.microsoft.com/en-us/windows-hardware/drivers/image/wia-item-categories
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
Set-StrictMode -Version 2

function Get-WiaProperty($owner, [int] $id) {
    foreach ($property in $owner.Properties) {
        if ($property.PropertyID -eq $id) { return $property }
    }
    return $null
}

function Get-WiaValue($owner, [int] $id, $fallback) {
    $property = Get-WiaProperty $owner $id
    if ($null -ne $property) { return $property.Value }
    return $fallback
}

function Set-WiaValue($owner, [int] $id, [int] $value, [bool] $required = $false) {
    $property = Get-WiaProperty $owner $id
    if ($null -eq $property) {
        if ($required) { throw "The WIA driver does not expose required scanner property $id." }
        return $false
    }
    try { $property.Value = $value } catch {
        throw "Unsupported scanner setting: the WIA driver rejected property $id = $value. Choose a supported resolution, mode, or source. $($_.Exception.Message)"
    }
    return $true
}

try {
    $manager = New-Object -ComObject WIA.DeviceManager
    $operation = $env:CAPTUREFAB_WIA_OPERATION
    if ($operation -eq 'discover') {
        $devices = @()
        foreach ($info in $manager.DeviceInfos) {
            # WiaDeviceType.ScannerDeviceType = 1.
            if ([int] $info.Type -ne 1) { continue }
            $name = [string] (Get-WiaValue $info 7 '')
            if ([string]::IsNullOrWhiteSpace($name)) { $name = [string] (Get-WiaValue $info 4 $info.DeviceID) }
            $devices += [pscustomobject] @{
                id = [string] $info.DeviceID
                name = $name
                vendor = [string] (Get-WiaValue $info 3 '')
                # WIA does not standardize a scanner serial-number property.
                serial = ''
            }
        }
        $json = ConvertTo-Json -InputObject @($devices) -Compress -Depth 4
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($json)
        $stdout = [Console]::OpenStandardOutput()
        $stdout.Write($bytes, 0, $bytes.Length)
        $stdout.Flush()
        exit 0
    }
    if ($operation -ne 'scan') { throw "Unknown WIA scanner operation: $operation" }

    $deviceId = $env:CAPTUREFAB_WIA_DEVICE
    $dpi = [int] $env:CAPTUREFAB_WIA_DPI
    $mode = $env:CAPTUREFAB_WIA_MODE
    $source = $env:CAPTUREFAB_WIA_SOURCE
    if ($dpi -le 0) { throw 'Scanner resolution must be greater than zero.' }
    if ($mode -notin @('Color', 'Gray', 'Lineart')) { throw "Unsupported scan mode: $mode" }
    if ($source -notin @('Default', 'Flatbed', 'Feeder')) { throw "Unsupported scan source: $source" }

    $selectedInfo = $null
    foreach ($info in $manager.DeviceInfos) {
        if ([int] $info.Type -eq 1 -and [string] $info.DeviceID -eq $deviceId) {
            $selectedInfo = $info
            break
        }
    }
    if ($null -eq $selectedInfo) { throw "WIA scanner not found or removed: $deviceId" }
    $device = $selectedInfo.Connect()
    if ($device.Items.Count -lt 1) { throw 'The WIA scanner exposes no image acquisition items.' }

    # WIA 2 drivers expose flatbed/feeder as separate items. WIA 1 drivers
    # overload the first item and use root document-handling selection (3088).
    $flatbedCategory = '{FB607B1F-43F3-488B-855B-FB703EC342A6}'
    $feederCategory = '{FE131934-F84C-42AD-8DA4-6129CDDD7288}'
    $item = $device.Items.Item(1)
    if ($source -ne 'Default') {
        $wantedCategory = if ($source -eq 'Feeder') { $feederCategory } else { $flatbedCategory }
        $matchingItem = $null
        foreach ($candidate in $device.Items) {
            $category = [string] (Get-WiaValue $candidate 4125 '')
            if ($category -eq $wantedCategory) { $matchingItem = $candidate; break }
        }
        if ($null -ne $matchingItem) {
            $item = $matchingItem
        } else {
            $selection = if ($source -eq 'Feeder') { 1 } else { 2 }
            if (-not (Set-WiaValue $device 3088 $selection)) {
                throw "Unsupported scanner source: the WIA driver does not support $source. Try Default or install the scanner manufacturer's WIA driver."
            }
            # Some legacy drivers rebuild their item collection on selection.
            $item = $device.Items.Item(1)
        }
    }

    # A saved WIA 1 duplex flag can otherwise acquire the back side as an
    # extra page. Preserve its current input source, including Default.
    $legacyHandling = Get-WiaProperty $device 3088
    if ($null -ne $legacyHandling) {
        $handling = [int] $legacyHandling.Value
        if (($handling -band 4) -ne 0) {
            # Clear DUPLEX, FRONT_FIRST, BACK_FIRST and BACK_ONLY together;
            # the ordering/back-side flags are valid only during duplex.
            $null = Set-WiaValue $device 3088 ($handling -band (-bnot 92))
        }
    }

    $intent = switch ($mode) { 'Color' { 1 }; 'Gray' { 2 }; 'Lineart' { 4 } }
    $null = Set-WiaValue $item 6146 $intent $true
    $dataType = switch ($mode) { 'Color' { 3 }; 'Gray' { 2 }; 'Lineart' { 0 } }
    $null = Set-WiaValue $item 4103 $dataType
    $null = Set-WiaValue $item 6147 $dpi $true
    $null = Set-WiaValue $item 6148 $dpi $true
    # A capture is one page, including when the driver's current source is ADF.
    $hasPages = Set-WiaValue $item 3096 1
    if (-not $hasPages) { $null = Set-WiaValue $device 3096 1 }
    # On WIA 2 feeder items FRONT_ONLY = 0x20 disables a saved duplex setting.
    $category = [string] (Get-WiaValue $item 4125 '')
    if ($category -eq $feederCategory) { $null = Set-WiaValue $item 3088 32 }

    # Request BMP; Transfer can return a driver's preferred format instead.
    # System.Drawing decodes BMP/JPEG/PNG/TIFF, including indexed scan images.
    $image = $item.Transfer('{B96B3CAB-0728-11D3-9D7B-0000F81EF32E}')
    if ($null -eq $image) { throw 'The WIA driver returned no scanned image.' }
    # Bound the decoded RGB bitmap before System.Drawing allocates it, even
    # when the eventual PNM is grayscale. This matches the Rust frame limit.
    if ([long] $image.Width * [long] $image.Height * 3 -gt 128MB) {
        throw 'The scanned page exceeds the supported image size. Choose a lower resolution.'
    }
    [byte[]] $imageBytes = $image.FileData.BinaryData
    Add-Type -AssemblyName System.Drawing
    Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;

public static class CaptureFabWiaPnm
{
    public static void Write(byte[] bytes, string mode)
    {
        using (var input = new MemoryStream(bytes, false))
        using (var image = Image.FromStream(input, true, true))
        {
            bool color = mode == "Color";
            int channels = color ? 3 : 1;
            if (image.Width <= 0 || image.Height <= 0 ||
                (long)image.Width * image.Height * 3 > 128L * 1024 * 1024)
                throw new InvalidDataException("The scanned page exceeds the supported image size.");
            // Draw into RGB to handle palette/grayscale/1-bit source bitmaps.
            using (var bitmap = new Bitmap(image.Width, image.Height, PixelFormat.Format24bppRgb))
            {
                using (var graphics = Graphics.FromImage(bitmap))
                    graphics.DrawImageUnscaled(image, 0, 0);
                var area = new Rectangle(0, 0, bitmap.Width, bitmap.Height);
                var data = bitmap.LockBits(area, ImageLockMode.ReadOnly, PixelFormat.Format24bppRgb);
                try
                {
                    var output = Console.OpenStandardOutput();
                    byte[] header = Encoding.ASCII.GetBytes((color ? "P6\n" : "P5\n") +
                        bitmap.Width + " " + bitmap.Height + "\n255\n");
                    output.Write(header, 0, header.Length);
                    byte[] bgr = new byte[checked(bitmap.Width * 3)];
                    byte[] row = new byte[checked(bitmap.Width * channels)];
                    for (int y = 0; y < bitmap.Height; y++)
                    {
                        Marshal.Copy(IntPtr.Add(data.Scan0, checked(y * data.Stride)), bgr, 0, bgr.Length);
                        for (int x = 0; x < bitmap.Width; x++)
                        {
                            int p = x * 3;
                            if (color)
                            {
                                row[p] = bgr[p + 2];
                                row[p + 1] = bgr[p + 1];
                                row[p + 2] = bgr[p];
                            }
                            else
                            {
                                int gray = (77 * bgr[p + 2] + 150 * bgr[p + 1] + 29 * bgr[p] + 128) >> 8;
                                row[x] = mode == "Lineart" ? (byte)(gray >= 128 ? 255 : 0) : (byte)gray;
                            }
                        }
                        output.Write(row, 0, row.Length);
                    }
                    output.Flush();
                }
                finally { bitmap.UnlockBits(data); }
            }
        }
    }
}
'@
    [CaptureFabWiaPnm]::Write($imageBytes, $mode)
    exit 0
} catch {
    [Console]::Error.WriteLine("WIA scanner: " + $_.Exception.Message)
    exit 1
}
