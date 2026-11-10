// Checks an archive's Sparkle EdDSA signature against the app's SUPublicEDKey.
// Usage: swift scripts/verify-update.swift <public-key> <signature> <archive>
import CryptoKit
import Foundation

let arguments = CommandLine.arguments
guard arguments.count == 4, let raw = Data(base64Encoded: arguments[1]),
    let signature = Data(base64Encoded: arguments[2]),
    let key = try? Curve25519.Signing.PublicKey(rawRepresentation: raw),
    let archive = try? Data(contentsOf: URL(fileURLWithPath: arguments[3]), options: .mappedIfSafe)
else { exit(2) }
exit(key.isValidSignature(signature, for: archive) ? 0 : 1)
