using System;
using System.Linq;
using System.Collections.Generic;
using System.Collections.ObjectModel;
using System.Text.Json;
using System.Text;
using System.Text.RegularExpressions;
using System.Runtime.InteropServices;
public static class FavoriteDevices { public static bool Contains(string address) => false; }
public enum Visibility { Visible, Collapsed }
public static class Loc { public static string T(string key) => key; }
public sealed record DeviceRow : System.ComponentModel.INotifyPropertyChanged
{
    public string Address { get; init; } = "";

    /// <summary>What the row is called, never empty.</summary>
    /// <remarks>
    /// A row carrying an empty name renders as an icon, a gap and two buttons,
    /// which reads as a broken list rather than as a device whose advertisement
    /// happened to be nameless. The address is always known - it is how the row
    /// exists at all - so it is the honest fallback, and it is applied here so
    /// no caller can forget it.
    /// </remarks>
    public string Name
    {
        get => string.IsNullOrWhiteSpace(_name) ? Address : _name;
        init => _name = value;
    }

    private readonly string _name = "";
    public bool Paired { get; init; }
    public bool LeAudio { get; init; }
    public bool Connected { get; init; }
    public bool Streaming { get; init; }
    public bool Connecting { get; init; }
    private int _rssi;
    public int Rssi { get => _rssi; init => _rssi = value; }
    public event System.ComponentModel.PropertyChangedEventHandler? PropertyChanged;

    public void UpdateSignal(int rssi)
    {
        if (_rssi == rssi) return;
        _rssi = rssi;
        PropertyChanged?.Invoke(this, new System.ComponentModel.PropertyChangedEventArgs(nameof(Signal)));
    }

    /// <summary>Segoe Fluent Icons: a speaker for audio, the Bluetooth mark otherwise.</summary>
    public string Glyph => LeAudio ? "\uE767" : "\uE702";

    public string Detail => Connecting
        ? Loc.T("device.connecting")
        : Connected
            ? (Streaming ? Loc.T("device.connected_playing") : Loc.T("device.connected"))
            : Paired ? Loc.T("device.paired") : LeAudio ? "LE Audio" : "Bluetooth LE";

    public string Badge => Streaming ? Loc.T("device.playing") : Loc.T("device.connected");

    public Visibility BadgeVisibility => Connected ? Visibility.Visible : Visibility.Collapsed;

    public string Signal => Rssi == 0 ? "" : $"{Rssi} dBm";

    /// <summary>
    /// A known device is connected; an unknown one is paired. Saying "Connect"
    /// for something that will run a full key exchange loses the user's trust.
    /// </summary>
    public string ActionLabel => Connected ? Loc.T("device.disconnect") : Paired ? Loc.T("device.connect") : Loc.T("device.pair");
    public string UnpairLabel => Loc.T("devices.unpair");
    public string StarGlyph => FavoriteDevices.Contains(Address) ? "★" : "☆";
    public string StarHint => Loc.T("device.star_hint");
    public void RefreshStar() => PropertyChanged?.Invoke(this, new System.ComponentModel.PropertyChangedEventArgs(nameof(StarGlyph)));


    public DeviceRow With(
        string? name = null,
        bool? leAudio = null,
        bool? connected = null,
        bool? streaming = null,
        bool? connecting = null,
        bool? paired = null,
        int? rssi = null) => new()
    {
        Address = Address,
        Name = name ?? Name,
        LeAudio = leAudio ?? LeAudio,
        Paired = paired ?? Paired,
        Rssi = rssi ?? Rssi,
        Connected = connected ?? Connected,
        Streaming = streaming ?? Streaming,
        Connecting = connecting ?? Connecting,
    };
}

public sealed record AdapterChoice(string Name, string InstanceId, string HardwareId,
    string Service, string Driver, bool Supported)
{
    /// <summary>
    /// The label in the adapter menu, saying outright when the driver package
    /// has never heard of this one.
    /// </summary>
    public override string ToString() => Supported
        ? $"{Name}  ·  {HardwareId}"
        : $"{Name}  ·  {HardwareId}  ·  {Loc.T("setup.not_listed")}";
}


public class Program {
    private readonly ObservableCollection<DeviceRow> _found = new();
    private readonly ObservableCollection<DeviceRow> _paired = new();
    private readonly Dictionary<string,string> _discoveredNames = new(StringComparer.OrdinalIgnoreCase);
    private static string Text(JsonElement j, string key) => j.GetProperty(key).GetString() ?? "";
    private void Report(string address, string name, int rssi = -50, bool le = false) {
        using var json = JsonDocument.Parse(JsonSerializer.Serialize(new { address, name, rssi, leAudio=le, paired=false }));
        AddDevice(json.RootElement);
    }
    private static void Check(bool ok, string message) { if (!ok) throw new Exception(message); }
    public static void Main() {
        var adapters = EnumerateSupportedAdapters(new[]{@"USB\VID_0B05&PID_1D70"});
        foreach(var adapter in adapters) Console.WriteLine($"PnP: {adapter.HardwareId} / {adapter.Service}");
        var p = new Program();
        p.Report("A", "(bez jmena)"); p.Report("B", "(unnamed)"); p.Report("C", "Speaker");
        Check(string.Join("", p._found.Select(x=>x.Address)) == "CAB", "Named first");
        p.Report("B", "Headphones", le:true);
        Check(string.Join("", p._found.Select(x=>x.Address)) == "CBA", "One promotion on name discovery");
        var row = p._found[1]; int changes=0;
        row.PropertyChanged += (_, e) => { if(e.PropertyName == "Signal") changes++; };
        for(int i=0;i<100;i++) { p.Report("b", i%2==0 ? "(bez jmena)" : "", -40-i%20); p.Report("A", "", -1); }
        Check(ReferenceEquals(row,p._found[1]), "RSSI must retain the row instance");
        Check(row.Name == "Headphones" && row.LeAudio, "Nameless packets must preserve identity and capabilities");
        Check(changes>0, "Signal changes must notify the binding");
        Check(string.Join("",p._found.Select(x=>x.Address)) == "CBA", "No signal-driven reorder or duplicates");
        p._found.Clear(); p.Report("B", "(bez jmena)");
        Check(p._found[0].Name == "Headphones", "Name survives a new scan");
        p._paired.Add(new DeviceRow { Address="P",Name="Paired",Paired=true,LeAudio=true });
        var paired=p._paired[0]; p.Report("P", "(unnamed)", -45);
        Check(ReferenceEquals(paired,p._paired[0]) && paired.Name=="Paired", "Paired RSSI update retains row and name");
        Console.WriteLine("PASS: named grouping, stable order, promotion, sticky names/LE, case-insensitive address, signal notification, row reuse, rescan, paired row");
    }
    private void AddDevice(JsonElement message)
    {
        var address = Text(message, "address");
        var alreadyKnown = _paired.Any(p => AddressesMatch(p.Address, address));

        // A paired device is already listed above; showing it twice is noise.
        // Its signal strength is still worth having, so it is folded in.
        if (alreadyKnown)
        {
            var reported = Text(message, "name");
            Update(address, row => row.With(
                name: UsefulDeviceName(reported, address) ? reported : row.Name,
                leAudio: message.GetProperty("leAudio").GetBoolean() || row.LeAudio,
                rssi: message.GetProperty("rssi").GetInt32()));
            return;
        }

        var existing = _found.FirstOrDefault(d => AddressesMatch(d.Address, address));
        var reportedName = Text(message, "name");
        if (UsefulDeviceName(reportedName, address)) _discoveredNames[address] = reportedName.Trim();
        var name = _discoveredNames.GetValueOrDefault(address) ?? existing?.Name ?? address;
        var newRow = (existing ?? new DeviceRow { Address = address }).With(
            name: name,
            rssi: message.GetProperty("rssi").GetInt32(),
            leAudio: message.GetProperty("leAudio").GetBoolean() || existing?.LeAudio == true,
            paired: message.GetProperty("paired").GetBoolean() || existing?.Paired == true);

        if (existing is not null)
        {
            var oldIndex = _found.IndexOf(existing);
            // Most advertisements only change RSSI. Update that text in place,
            // preserving the row, hover/focus and the button under the pointer.
            if (existing.Name == newRow.Name && existing.LeAudio == newRow.LeAudio &&
                existing.Paired == newRow.Paired)
            {
                existing.UpdateSignal(newRow.Rssi);
                return;
            }
            _found[oldIndex] = newRow;
            if (Rank(existing) == Rank(newRow)) return;
            var target = _found.Take(oldIndex).Count(row => Rank(row) <= Rank(newRow));
            if (target != oldIndex) _found.Move(oldIndex, target);
            return;
        }

        // Named devices first; discovery order stays stable within each group.
        // Signal strength and subsequent advertisements never reorder the list.
        var insertAt = 0;
        while (insertAt < _found.Count && Rank(_found[insertAt]) <= Rank(newRow)) insertAt++;
        _found.Insert(insertAt, newRow);
    }

    private static int Rank(DeviceRow row) => UsefulDeviceName(row.Name, row.Address) ? 0 : 1;

    private static bool AddressesMatch(string a, string b) =>
        string.Equals(a, b, StringComparison.OrdinalIgnoreCase);

    /// <summary>Applies a change to whichever list holds this device.</summary>
    private void Update(string? address, Func<DeviceRow, DeviceRow> change)
    {
        if (string.IsNullOrEmpty(address))
        {
            return;
        }

        foreach (var list in new[] { _paired, _found })
        {
            for (var i = 0; i < list.Count; i++)
            {
                if (AddressesMatch(list[i].Address, address))
                {
                    var current = list[i];
                    var updated = change(current);
                    if (current.Name == updated.Name && current.LeAudio == updated.LeAudio &&
                        current.Paired == updated.Paired && current.Connected == updated.Connected &&
                        current.Connecting == updated.Connecting && current.Streaming == updated.Streaming)
                        current.UpdateSignal(updated.Rssi);
                    else list[i] = updated;
                }
            }
        }
    }

    private static bool UsefulDeviceName(string name, string address) =>
        !string.IsNullOrWhiteSpace(name) &&
        !string.Equals(name, address, StringComparison.OrdinalIgnoreCase) &&
        !name.Contains("unnamed", StringComparison.OrdinalIgnoreCase) &&
        !name.Contains("bez jmena", StringComparison.OrdinalIgnoreCase) &&
        !name.Contains("bez jména", StringComparison.OrdinalIgnoreCase);

    private const uint DigcfPresent = 0x00000002;
    private const uint DigcfAllClasses = 0x00000004;
    private const uint SpdrpDeviceDesc = 0x00000000;
    private const uint SpdrpCompatibleIds = 0x00000002;
    private const uint SpdrpService = 0x00000004;
    private const uint SpdrpFriendlyName = 0x0000000C;

    [StructLayout(LayoutKind.Sequential)]
    private struct SpDevinfoData
    {
        public uint Size;
        public Guid ClassGuid;
        public uint DevInst;
        public nint Reserved;
    }

    [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern nint SetupDiGetClassDevs(nint classGuid, string? enumerator,
        nint parent, uint flags);

    [DllImport("setupapi.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool SetupDiEnumDeviceInfo(nint set, uint index, ref SpDevinfoData data);

    [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool SetupDiGetDeviceInstanceId(nint set, ref SpDevinfoData data,
        StringBuilder instanceId, int size, out int required);

    [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool SetupDiGetDeviceRegistryProperty(nint set, ref SpDevinfoData data,
        uint property, out uint type, byte[] buffer, uint size, out uint required);

    [DllImport("setupapi.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool SetupDiDestroyDeviceInfoList(nint set);

    private static List<AdapterChoice> EnumerateSupportedAdapters(string[] supportedIds)
    {
        var result = new List<AdapterChoice>();
        var set = SetupDiGetClassDevs(0, null, 0, DigcfPresent | DigcfAllClasses);
        if (set == -1) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
        try
        {
            for (uint index = 0; ; index++)
            {
                var data = new SpDevinfoData { Size = (uint)Marshal.SizeOf<SpDevinfoData>() };
                if (!SetupDiEnumDeviceInfo(set, index, ref data))
                {
                    if (Marshal.GetLastWin32Error() == 259) break; // ERROR_NO_MORE_ITEMS
                    continue;
                }
                var instanceBuffer = new StringBuilder(512);
                if (!SetupDiGetDeviceInstanceId(set, ref data, instanceBuffer,
                        instanceBuffer.Capacity, out _)) continue;
                var instance = instanceBuffer.ToString();
                var hardwareId = supportedIds.FirstOrDefault(id =>
                    instance.StartsWith(id, StringComparison.OrdinalIgnoreCase));

                // Not in the INF, but still worth listing if it is a Bluetooth
                // controller. Leaving those out meant somebody with a different
                // dongle saw an empty menu and no way to tell "the adapter is
                // not plugged in" from "the adapter is here and this program
                // has never heard of it" - which are opposite problems with
                // opposite fixes.
                var supported = hardwareId is not null;
                if (!supported)
                {
                    var compatible = SetupDeviceProperty(set, ref data, SpdrpCompatibleIds);
                    if (!compatible.Contains("Class_E0&SubClass_01&Prot_01",
                            StringComparison.OrdinalIgnoreCase))
                    {
                        continue;
                    }
                    hardwareId = BareHardwareId(instance);
                    if (hardwareId is null) continue;
                }

                if (hardwareId is null) continue;

                var name = SetupDeviceProperty(set, ref data, SpdrpFriendlyName);
                var description = SetupDeviceProperty(set, ref data, SpdrpDeviceDesc);
                var service = SetupDeviceProperty(set, ref data, SpdrpService);
                result.Add(new AdapterChoice(
                    string.IsNullOrWhiteSpace(name) ? description : name,
                    instance, hardwareId, service, description, supported));
            }
        }
        finally
        {
            SetupDiDestroyDeviceInfoList(set);
        }
        return result;
    }

    /// <summary>The plain USB\VID_xxxx&amp;PID_xxxx part of a device instance id.</summary>
    /// <remarks>
    /// The form an INF matches on. The rest of the instance id names one
    /// physical port and one firmware revision, neither of which belongs in a
    /// driver package.
    /// </remarks>
    private static string? BareHardwareId(string instance)
    {
        var match = Regex.Match(instance, @"^USB\\VID_[0-9A-Fa-f]{4}&PID_[0-9A-Fa-f]{4}",
            RegexOptions.IgnoreCase);
        return match.Success ? match.Value.ToUpperInvariant() : null;
    }

    private static string SetupDeviceProperty(nint set, ref SpDevinfoData data, uint property)
    {
        var buffer = new byte[2048];
        if (!SetupDiGetDeviceRegistryProperty(set, ref data, property, out _, buffer,
                (uint)buffer.Length, out _)) return "";
        return Encoding.Unicode.GetString(buffer).TrimEnd('\0');
    }

}