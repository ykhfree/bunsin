# Bunsin.app for macOS

`packaging/macos/build-app.sh` builds `target/package/Bunsin.app` from `bunsin-app`:
`Contents/MacOS/bunsin-app`, `Contents/Info.plist` and `Contents/PkgInfo`. The repository has no
icon yet, so macOS shows its default icon.

```sh
BUNSIN_BUNDLE_ID=com.example.bunsin packaging/macos/build-app.sh
open target/package/Bunsin.app
```

`BUNSIN_BUNDLE_ID` sets the bundle identifier. The default `local.bunsin.app` is for builds on
your own Mac only. The window keeps its access token in the login Keychain; a new identifier or
a new signature can make macOS ask for Keychain access again.

## The bundle is not signed

The script does not sign or notarize. macOS Gatekeeper blocks an unsigned app that came from
another computer. On the Mac that built it, the app opens. To give the app to other people,
sign and notarize it by hand with an Apple Developer ID:

1. Sign the bundle with the hardened runtime:
   `codesign --force --options runtime --timestamp --sign "Developer ID Application: <name> (<team>)" target/package/Bunsin.app`
2. Check the signature: `codesign --verify --strict --verbose=2 target/package/Bunsin.app`.
3. Make a zip for the notary service:
   `ditto -c -k --keepParent target/package/Bunsin.app target/package/Bunsin.zip`.
4. Send it and wait for the result:
   `xcrun notarytool submit target/package/Bunsin.zip --keychain-profile <profile> --wait`.
   `xcrun notarytool store-credentials` makes the keychain profile once.
5. Staple the ticket to the bundle: `xcrun stapler staple target/package/Bunsin.app`.
6. Check it as Gatekeeper does: `spctl --assess --type execute --verbose target/package/Bunsin.app`.

Nobody has signed or notarized Bunsin.app yet. The app may need entitlements under the hardened
runtime; nobody has checked that.
