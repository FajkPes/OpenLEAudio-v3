"""Run the production discovery methods without loading WinUI (dotnet 8 required)."""
from pathlib import Path
import subprocess

root = Path(__file__).resolve().parents[1]
source = (root / "app/OpenLEAudio/MainWindow.xaml.cs").read_text(encoding="utf-8-sig")
output = root / "tests/.discovery-regression"
output.mkdir(exist_ok=True)
model = source[source.index("public sealed record DeviceRow"):source.index("public sealed record AdapterChoice")]
methods = source[source.index("    private void AddDevice("):source.index("    // --------------------------------------------------------------- settings", source.index("    private void AddDevice("))]
useful = source[source.index("    private static bool UsefulDeviceName"):source.index("    private FrameworkElement BuildSection")]
header = """using System;
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
"""
adapter_model = source[source.index("public sealed record AdapterChoice"):source.index("public sealed partial class MainWindow")]
adapter_methods = source[source.index("    private const uint DigcfPresent"):source.index("    private void SetupAdapterChanged")]
harness = """
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
        var adapters = EnumerateSupportedAdapters(new[]{@"USB\\VID_0B05&PID_1D70"});
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
"""
(output / "Program.cs").write_text(header+model+adapter_model+harness+methods+useful+adapter_methods+"}",encoding="utf-8")
(output / "Regression.csproj").write_text('<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><OutputType>Exe</OutputType><TargetFramework>net8.0</TargetFramework><Nullable>enable</Nullable></PropertyGroup></Project>')
(output / "NuGet.Config").write_text('<configuration><packageSources><clear /></packageSources></configuration>')
subprocess.run(["dotnet","restore",str(output / "Regression.csproj"),"--configfile",str(output / "NuGet.Config")],check=True)
subprocess.run(["dotnet","run","--no-restore","--project",str(output / "Regression.csproj"),"-c","Release"],check=True)
