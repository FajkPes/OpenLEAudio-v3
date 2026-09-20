using System;
using System.Collections.Generic;
using System.IO;
using System.Text.Json;
namespace OpenLEAudio;
internal static class FavoriteDevices
{
    private static readonly string FilePath = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "OpenLEAudio", "favorites.json");
    private static HashSet<string> Read()
    {
        try { return new(JsonSerializer.Deserialize<string[]>(File.ReadAllText(FilePath)) ?? Array.Empty<string>(), StringComparer.OrdinalIgnoreCase); }
        catch { return new(StringComparer.OrdinalIgnoreCase); }
    }
    public static bool Contains(string address) => Read().Contains(address);
    public static void Toggle(string address)
    {
        var items = Read(); if (!items.Remove(address)) items.Add(address);
        Directory.CreateDirectory(Path.GetDirectoryName(FilePath)!);
        var temporary = FilePath + ".tmp";
        File.WriteAllText(temporary, JsonSerializer.Serialize(items));
        File.Move(temporary, FilePath, true);
    }
}
