import Foundation

/// How an app is customised and signed. Saved with each library entry, so a refresh repeats it exactly.
struct AppOptions: Codable, Equatable {
    var name = ""
    var bundleId = ""
    var version = ""
    var iconPath = ""
    var minOS = ""
    var removeDeviceLimits = false
    var lowerMinOS = false
    var fileSharing = false
    var iPadFullscreen = false
    var gameMode = false
    var proMotion = false
    var liquidGlass = false
    var removeURLSchemes = false
    var removeExtensions = false
    var removeWatch = false
    /// Add the tweak loader even when there are no tweaks (named for ElleKit, the only loader there used to be).
    var elleKit = false
    var tweakLoader: TweakLoader = .elleKit
    /// Your own CydiaSubstrate.framework, for `tweakLoader == .substrate`.
    var substratePath = ""
    var tweaks: [String] = []
    var replaceFiles: [String: String] = [:]
    /// Add your team ID to the bundle ID (Sideloadly's "automatic bundle ID"). Off keeps the app's own ID.
    var autoBundleId = true
    /// Add get-task-allow, for debugging and JIT. Apple ID signing always has it; this matters for ad-hoc.
    var enableDebugging = false

    var engineJSON: [String: Any] {
        var o: [String: Any] = [
            "remove_device_limits": removeDeviceLimits, "lower_min_os": lowerMinOS, "file_sharing": fileSharing,
            "ipad_fullscreen": iPadFullscreen, "game_mode": gameMode, "pro_motion": proMotion,
            "liquid_glass": liquidGlass, "remove_url_schemes": removeURLSchemes,
            "remove_extensions": removeExtensions, "remove_watch": removeWatch, "ellekit": elleKit,
            "tweak_framework": tweakLoader.rawValue, "tweaks": tweaks, "replace_files": replaceFiles,
            "auto_bundle_id": autoBundleId, "debugging": enableDebugging,
        ]
        if !name.isEmpty { o["name"] = name }
        if !bundleId.isEmpty { o["bundle_id"] = bundleId }
        if !version.isEmpty { o["version"] = version }
        if !iconPath.isEmpty { o["icon"] = iconPath }
        if !minOS.isEmpty { o["min_os"] = minOS }
        if !substratePath.isEmpty { o["substrate_path"] = substratePath }
        return o
    }

    /// Options that change something, for the little summary under each app.
    var summary: [String] {
        var s: [String] = []
        if !name.isEmpty { s.append("Renamed") }
        if !iconPath.isEmpty { s.append("Custom icon") }
        if fileSharing { s.append("File sharing") }
        if removeDeviceLimits { s.append("No device limits") }
        if removeExtensions { s.append("No extensions") }
        if proMotion { s.append("120 Hz") }
        if gameMode { s.append("Game Mode") }
        if !tweaks.isEmpty { s.append("\(tweaks.count) tweak\(tweaks.count == 1 ? "" : "s")") }
        if !tweaks.isEmpty && tweakLoader != .elleKit { s.append(tweakLoader.shortLabel) }
        if !autoBundleId && bundleId.isEmpty { s.append("Original bundle ID") }
        if enableDebugging { s.append("Debugging") }
        return s
    }
}

extension AppOptions {
    /// Reads every field as optional, so a library saved by an older Sideport (before an option existed) still
    /// loads with that option's default, instead of failing to decode and coming back empty.
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        func get<T: Decodable>(_ key: CodingKeys, _ fallback: T) -> T {
            (try? c.decodeIfPresent(T.self, forKey: key)) ?? fallback
        }
        let d = AppOptions()
        self.init()
        name = get(.name, d.name)
        bundleId = get(.bundleId, d.bundleId)
        version = get(.version, d.version)
        iconPath = get(.iconPath, d.iconPath)
        minOS = get(.minOS, d.minOS)
        removeDeviceLimits = get(.removeDeviceLimits, d.removeDeviceLimits)
        lowerMinOS = get(.lowerMinOS, d.lowerMinOS)
        fileSharing = get(.fileSharing, d.fileSharing)
        iPadFullscreen = get(.iPadFullscreen, d.iPadFullscreen)
        gameMode = get(.gameMode, d.gameMode)
        proMotion = get(.proMotion, d.proMotion)
        liquidGlass = get(.liquidGlass, d.liquidGlass)
        removeURLSchemes = get(.removeURLSchemes, d.removeURLSchemes)
        removeExtensions = get(.removeExtensions, d.removeExtensions)
        removeWatch = get(.removeWatch, d.removeWatch)
        elleKit = get(.elleKit, d.elleKit)
        tweakLoader = get(.tweakLoader, d.tweakLoader)
        substratePath = get(.substratePath, d.substratePath)
        tweaks = get(.tweaks, d.tweaks)
        replaceFiles = get(.replaceFiles, d.replaceFiles)
        autoBundleId = get(.autoBundleId, d.autoBundleId)
        enableDebugging = get(.enableDebugging, d.enableDebugging)
    }
}

/// What makes injected tweaks work.
enum TweakLoader: String, Codable, CaseIterable, Identifiable {
    case elleKit = "ellekit", substrate, none
    var id: String { rawValue }
    var label: String {
        switch self {
        case .elleKit: return "ElleKit (built in)"
        case .substrate: return "Cydia Substrate (your own copy)"
        case .none: return "None"
        }
    }
    var shortLabel: String {
        switch self {
        case .elleKit: return "ElleKit"
        case .substrate: return "Substrate"
        case .none: return "No loader"
        }
    }
}

struct IPAInfo: Equatable {
    var path: String
    var name: String
    var bundleId: String
    var version: String
    var build: String
    var minOS: String
    var size: Int64
    var modified: Date?
    var icon: String?
    var extensions: [String]
    var hasWatch: Bool
    var deviceLimited: Bool
    var fileSharing: Bool

    init(_ d: [String: Any]) {
        path = d["path"] as? String ?? ""
        name = d["name"] as? String ?? ""
        bundleId = d["bundle_id"] as? String ?? ""
        version = d["version"] as? String ?? ""
        build = d["build"] as? String ?? ""
        minOS = d["min_os"] as? String ?? ""
        size = (d["size"] as? NSNumber)?.int64Value ?? 0
        modified = (d["modified"] as? String).flatMap(parseDate)
        icon = d["icon"] as? String
        extensions = d["extensions"] as? [String] ?? []
        hasWatch = d["has_watch"] as? Bool ?? false
        deviceLimited = d["device_limited"] as? Bool ?? false
        fileSharing = d["file_sharing"] as? Bool ?? false
    }
}

enum Signing: String, Codable, CaseIterable, Identifiable {
    case appleID = "apple_id", certificate, adhoc
    var id: String { rawValue }
    var label: String {
        switch self {
        case .appleID: return "Apple ID"
        case .certificate: return "My own certificate"
        case .adhoc: return "Ad-hoc (jailbroken / TrollStore)"
        }
    }
}

/// One sideloaded app: where its IPA lives, how it's customised, and when it was last signed.
struct LibraryApp: Codable, Identifiable, Equatable {
    var id = UUID()
    var ipaPath: String
    var backupIPAPath: String?           // e.g. Sideloadly's cached copy, used if ipaPath is gone
    var originalName: String
    var originalBundleId: String
    var version: String
    var iconPath: String?
    var options = AppOptions()
    var signing: Signing = .appleID
    var certFiles: [String] = []
    var profilePath: String?
    var deviceUDID: String?
    var deviceName: String?
    var account: String?
    var installedBundleId: String?       // the final ID on the phone, kept on refresh so app data stays
    var installedVersion: String?
    var lastSigned: Date?
    var expires: Date?
    var ipaModifiedAtSign: Date?
    var lastError: String?
    var autoRefresh = true
    var pinned = false
    var importedFrom: String?

    var displayName: String { options.name.isEmpty ? originalName : options.name }
    /// False for IPAs that are only in the library folder so far.
    var isInstalled: Bool { lastSigned != nil || installedBundleId != nil }
    var shownIcon: String? { options.iconPath.isEmpty ? iconPath : options.iconPath }

    var sourcePath: String? {
        if FileManager.default.fileExists(atPath: ipaPath) { return ipaPath }
        if let b = backupIPAPath, FileManager.default.fileExists(atPath: b) { return b }
        return nil
    }

    var daysLeft: Double? {
        guard let e = expires else { return nil }
        return e.timeIntervalSinceNow / 86_400
    }

    enum Health { case fresh, soon, urgent, expired, unknown }
    var health: Health {
        guard let d = daysLeft else { return .unknown }
        if d <= 0 { return .expired }
        if d < 1.5 { return .urgent }
        if d < 3.5 { return .soon }
        return .fresh
    }
}

struct DeviceInfo: Identifiable, Equatable, Hashable {
    var id: String { udid }
    var udid: String
    var name: String
    var model: String
    var ios: String
    /// "usb", "wifi" (seen by macOS over Wi-Fi) or "network" (Sideport reached it directly with its saved pairing).
    var connection: String
    /// "Show this iPhone when on Wi-Fi" is on. Only known while it's plugged in by USB.
    var wifiEnabled: Bool?
    /// Sideport has saved its pairing, so it can reach the phone over Wi-Fi even when macOS doesn't list it.
    var paired: Bool

    init(_ d: [String: Any]) {
        udid = d["udid"] as? String ?? ""
        name = d["name"] as? String ?? "iPhone"
        model = d["model"] as? String ?? ""
        ios = d["ios"] as? String ?? ""
        connection = d["connection"] as? String ?? "usb"
        wifiEnabled = d["wifi_enabled"] as? Bool
        paired = d["paired"] as? Bool ?? false
    }

    var wifi: Bool { connection != "usb" }
    var connectionLabel: String {
        switch connection {
        case "usb": return "USB"
        case "network": return "Wi-Fi (direct)"
        default: return "Wi-Fi"
        }
    }

    var symbol: String { model.hasPrefix("iPad") ? "ipad" : model.hasPrefix("AppleTV") ? "appletv" : "iphone" }
}

/// A free Apple ID's limit on new App IDs: `max` (10) every 7 days, `available` of them left now, and when the
/// oldest one in use expires and gives one back.
struct AppIdQuota: Equatable {
    var available: Int
    var max: Int
    var nextFree: Date?
}

struct AccountInfo: Identifiable, Equatable {
    var id: String { email }
    var email: String
    var firstName: String
    var team: String
    var selected: Bool

    init(_ d: [String: Any]) {
        email = d["email"] as? String ?? ""
        firstName = d["first_name"] as? String ?? ""
        team = d["team"] as? String ?? ""
        selected = d["selected"] as? Bool ?? false
    }
}

struct DeviceApp: Identifiable, Equatable {
    var id: String { bundleId }
    var bundleId: String
    var name: String
    var version: String
    var sideloaded: Bool

    init(_ d: [String: Any]) {
        bundleId = d["bundle_id"] as? String ?? ""
        name = d["name"] as? String ?? ""
        version = d["version"] as? String ?? ""
        let signer = d["signer"] as? String ?? ""
        sideloaded = signer.contains("Apple Development") || signer.isEmpty
    }
}

struct ActivityEntry: Codable, Identifiable {
    var id = UUID()
    var date = Date()
    var title: String
    var detail: String
    var ok: Bool
}

/// A running install or refresh, shown as a progress card.
struct Job: Identifiable, Equatable {
    var id: String
    var appId: UUID?
    var title: String
    var stage = "Starting"
    var percent = 0.0
}

/// A question from the engine the user has to answer.
struct Prompt: Identifiable, Equatable {
    var id: String
    var kind: String
    var method: String
    var phones: [(id: Int, label: String)]
    var message: String

    static func == (a: Prompt, b: Prompt) -> Bool { a.id == b.id }
}

func parseDate(_ s: String) -> Date? {
    let f = ISO8601DateFormatter()
    f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
    if let d = f.date(from: s) { return d }
    f.formatOptions = [.withInternetDateTime]
    return f.date(from: s)
}
