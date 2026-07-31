// SensorView bridge sidecar.
//
// Thin console wrapper around LibreHardwareMonitorLib: polls all hardware once
// per second and writes one JSON snapshot per line to stdout, in exactly the
// shape of SensorView's Rust model (src/model.rs). The Rust app spawns this
// process and reads the stream; it never links .NET itself.
//
// The update loop mirrors OpenHardwareMonitor's GUI/UpdateVisitor.cs pattern.
// Full sensor coverage (Super-I/O, MSR, SMBus) requires administrator rights;
// without them LibreHardwareMonitor silently exposes the subset it can reach.

using System.Security.Principal;
using System.Text.Json;
using LibreHardwareMonitor.Hardware;

var computer = new Computer
{
    IsCpuEnabled = true,
    IsGpuEnabled = true,
    IsMemoryEnabled = true,
    IsMotherboardEnabled = true,
    IsControllerEnabled = true,
    IsNetworkEnabled = true,
    IsStorageEnabled = true,
    IsPsuEnabled = true,
    IsBatteryEnabled = true,
};

computer.Open();

// First line: diagnostics meta so the Rust app can explain zero sensors
// (driver blocked / not elevated). ring0_report is the ring0 slice of LHM's
// own report, which names the exact WinRing0 open/install failure.
var isElevated = false;
try
{
    using var identity = WindowsIdentity.GetCurrent();
    isElevated = new WindowsPrincipal(identity).IsInRole(WindowsBuiltInRole.Administrator);
}
catch { /* ignore */ }

// Flush the tree on Ctrl+C / kill so the driver handle is released cleanly.
AppDomain.CurrentDomain.ProcessExit += (_, _) => computer.Close();
Console.CancelKeyPress += (_, _) => { computer.Close(); Environment.Exit(0); };

var json = new JsonSerializerOptions { WriteIndented = false };

// Use the raw stdout stream so a broken pipe (parent gone) surfaces as an
// IOException we can act on, instead of being swallowed by Console.Out.
using var stdout = Console.OpenStandardOutput();
using var writer = new StreamWriter(stdout) { AutoFlush = false };

// First line: diagnostics meta so the Rust app can explain zero sensors
// (driver blocked / not elevated). ring0_report is the ring0 slice of LHM's
// own report, which names the exact WinRing0 open/install failure.
var ring0Report = ExtractRing0(computer.GetReport());
var lhmVersion = typeof(Computer).Assembly.GetName().Version?.ToString() ?? "?";
writer.WriteLine(JsonSerializer.Serialize(new Dictionary<string, object?>
{
    ["meta"] = new Dictionary<string, object?>
    {
        ["lhm_version"] = lhmVersion,
        ["is_elevated"] = isElevated,
        ["ring0_report"] = ring0Report,
    },
}));
writer.Flush();

var visitor = new UpdateVisitor();

// Watch the parent (Rust app). If it dies, exit promptly so we never orphan —
// an elevated orphan would leak CPU and hold the driver handle.
var parentId = Environment.GetEnvironmentVariable("SENSORVIEW_PARENT_PID");
System.Diagnostics.Process? parent = null;
if (int.TryParse(parentId, out var pid))
{
    try { parent = System.Diagnostics.Process.GetProcessById(pid); } catch { }
}

// S.M.A.R.T. is emitted on its own slow cadence, not with every tick.
// Re-reading drive health at 1 Hz keeps disks out of low-power states and
// burns a limited log-read budget, which is the same reason SensorView keeps
// a separate "slow lane" (see src/inventory.rs). 0 forces the first pass to
// happen immediately so the UI is not blank for the first half-minute.
const int StorageEveryTicks = 30;
var tick = 0;

while (true)
{
    if (parent is { HasExited: true })
    {
        break;
    }
    computer.Accept(visitor);
    var tree = computer.Hardware.Select(MapHardware).ToList();
    try
    {
        writer.WriteLine(JsonSerializer.Serialize(tree, json));

        if (tick % StorageEveryTicks == 0)
        {
            // Its own line, tagged, so the reader can tell it from a tree
            // snapshot without inspecting the shape.
            var storage = CollectStorage(computer);
            if (storage.Count > 0)
            {
                writer.WriteLine(JsonSerializer.Serialize(
                    new Dictionary<string, object?> { ["storage"] = storage }, json));
            }
        }
        tick++;

        writer.Flush(); // throws if the parent closed the read end of the pipe
    }
    catch (IOException)
    {
        break; // parent gone → exit
    }
    catch (Exception ex)
    {
        // One bad snapshot must not take the process down: the app would lose
        // every sensor and silently fall back to demo data. Report it and keep
        // polling — the next tick is usually fine.
        Console.Error.WriteLine($"snapshot skipped: {ex.Message}");
    }
    Thread.Sleep(1000);
}

computer.Close();

// Pull the "Ring0" section out of LHM's full text report — it records whether
// the kernel driver opened, and any install/blocklist error.
static string ExtractRing0(string report)
{
    var lines = report.Replace("\r\n", "\n").Split('\n');
    var kept = new List<string>();
    var capturing = false;
    foreach (var line in lines)
    {
        if (line.StartsWith("Ring0", StringComparison.OrdinalIgnoreCase)
            || line.Contains("WinRing0")
            || line.Contains("Kernel Driver"))
        {
            capturing = true;
        }
        else if (capturing && line.Length > 0 && !char.IsWhiteSpace(line[0]) && line.Contains("Report"))
        {
            capturing = false;
        }
        if (capturing)
        {
            kept.Add(line);
        }
    }
    var text = string.Join("\n", kept).Trim();
    return text.Length == 0 ? "(no ring0 section in report)" : text;
}

/// <summary>
/// Per-drive identity and S.M.A.R.T. health, in the shape of the Rust
/// <c>model::storage::StorageHealth</c>.
///
/// Everything here comes from LibreHardwareMonitor's public surface:
/// <c>StorageDevice.Storage</c> (a DiskInfoToolkit <c>Storage</c>, carrying
/// identity and the decoded health summary) and <c>StorageDevice.Attributes</c>
/// (the raw attribute table). No reflection and no extra device handles — LHM
/// has already opened the drives.
///
/// Requires administrator rights: unelevated, LHM cannot open
/// <c>\\.\PhysicalDriveN</c> and enumerates no storage at all, so this returns
/// an empty list rather than partial data.
/// </summary>
static List<Dictionary<string, object?>> CollectStorage(Computer computer)
{
    var drives = new List<Dictionary<string, object?>>();

    foreach (var hw in computer.Hardware)
    {
        if (hw is not LibreHardwareMonitor.Hardware.Storage.StorageDevice dev)
        {
            continue;
        }

        try
        {
            var st = dev.Storage;
            var smart = st?.Smart;

            // The attribute table, with the vendor-decoded name where LHM has
            // one. `RawValueULong` is the 48-bit raw field the value actually
            // means; CurrentValue/WorstValue/Threshold are the normalised 0-255
            // triple every S.M.A.R.T. tool shows beside it.
            var attributes = new List<Dictionary<string, object?>>();
            foreach (var a in dev.Attributes)
            {
                var raw = a.Attribute?.Attribute;
                if (raw is null)
                {
                    continue;
                }
                attributes.Add(new Dictionary<string, object?>
                {
                    ["id"] = raw.Value.ID,
                    ["name"] = a.Attribute?.Info?.Name ?? a.Name ?? $"Attribute {raw.Value.ID}",
                    ["current"] = raw.Value.CurrentValue,
                    ["worst"] = raw.Value.WorstValue,
                    ["threshold"] = raw.Value.Threshold,
                    ["raw"] = raw.Value.RawValueULong,
                });
            }

            drives.Add(new Dictionary<string, object?>
            {
                ["identifier"] = hw.Identifier.ToString(),
                ["model"] = st?.Model ?? hw.Name ?? "",
                ["serial"] = st?.SerialNumber ?? "",
                ["firmware"] = st?.FirmwareRev ?? st?.Firmware ?? "",
                // The Rust side keys presentation off this, so send what the
                // bus actually reports rather than guessing from the model name.
                ["protocol"] = st?.IsNVMe == true ? "Nvme" : "Ata",
                ["is_ssd"] = st?.IsSSD,
                ["bus"] = st?.BusType.ToString(),
                ["capacity_bytes"] = st?.TotalSize,
                ["free_bytes"] = st?.TotalFreeSize,
                ["temperature_c"] = smart?.Temperature,
                // LHM exposes both a measured and a detected figure; the
                // measured one is the drive's own counter where it has one.
                ["power_on_hours"] = smart?.MeasuredPowerOnHours > 0
                    ? smart?.MeasuredPowerOnHours
                    : smart?.DetectedPowerOnHours,
                ["power_cycles"] = smart?.PowerOnCount,
                ["life_remaining_pct"] = smart?.Life,
                ["host_reads_gb"] = smart?.HostReads,
                ["host_writes_gb"] = smart?.HostWrites,
                ["nand_writes_gb"] = smart?.NandWrites,
                ["status"] = smart?.DiskStatus.ToString(),
                ["attributes"] = attributes,
            });
        }
        catch (Exception ex)
        {
            // One unreadable drive must not cost us the others, and must not
            // take down the sidecar — the app would lose every sensor.
            Console.Error.WriteLine($"storage skipped for {hw.Name}: {ex.Message}");
        }
    }

    return drives;
}

static Dictionary<string, object?> MapHardware(IHardware hw)
{
    return new Dictionary<string, object?>
    {
        ["identifier"] = hw.Identifier.ToString(),
        ["name"] = hw.Name,
        ["type"] = MapHardwareType(hw.HardwareType),
        ["sensors"] = hw.Sensors
            .OrderBy(s => s.SensorType).ThenBy(s => s.Index)
            .Select(MapSensor).ToList(),
        ["sub_hardware"] = hw.SubHardware.Select(MapHardware).ToList(),
    };
}

static Dictionary<string, object?> MapSensor(ISensor s)
{
    return new Dictionary<string, object?>
    {
        ["identifier"] = s.Identifier.ToString(),
        ["name"] = s.Name,
        ["type"] = s.SensorType.ToString(), // LHM names match the Rust enum 1:1
        ["index"] = s.Index,
        ["value"] = Finite(s.Value),
        ["min"] = (float?)null, // running stats are tracked on the Rust side
        ["max"] = (float?)null,
        ["avg"] = (float?)null,
    };
}

/// <summary>
/// Drop non-finite readings.
///
/// LHM occasionally yields NaN or ±Infinity — a rate computed over a zero time
/// delta, a divide by a missing denominator. System.Text.Json refuses to write
/// those and throws, which killed the whole sidecar mid-snapshot and left the
/// app with no sensors at all. A non-finite reading isn't a reading, so report
/// it as null, which the Rust model already expresses as Option&lt;f32&gt;::None.
/// </summary>
static float? Finite(float? v) => v.HasValue && float.IsFinite(v.Value) ? v : null;

// Rust model.rs HardwareType variant names (LHM names differ slightly).
static string MapHardwareType(HardwareType t) => t switch
{
    HardwareType.Motherboard => "Mainboard",
    HardwareType.SuperIO => "SuperIO",
    HardwareType.Cpu => "Cpu",
    HardwareType.Memory => "Ram",
    HardwareType.GpuNvidia => "GpuNvidia",
    HardwareType.GpuAmd => "GpuAti",
    HardwareType.GpuIntel => "GpuIntel",
    HardwareType.Storage => "Storage",
    HardwareType.Network => "Network",
    HardwareType.Cooler => "Cooler",
    HardwareType.EmbeddedController => "EmbeddedController",
    HardwareType.Psu => "Psu",
    HardwareType.Battery => "Battery",
    _ => "Mainboard",
};

/// <summary>Mirrors OpenHardwareMonitor's GUI/UpdateVisitor.cs.</summary>
class UpdateVisitor : IVisitor
{
    public void VisitComputer(IComputer computer) => computer.Traverse(this);

    public void VisitHardware(IHardware hardware)
    {
        hardware.Update();
        foreach (IHardware sub in hardware.SubHardware)
            sub.Accept(this);
    }

    public void VisitSensor(ISensor sensor) { }

    public void VisitParameter(IParameter parameter) { }
}
