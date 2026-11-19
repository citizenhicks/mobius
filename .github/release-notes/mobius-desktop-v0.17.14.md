# mobius-desktop 0.17.14

- Convert HEIC, HEIF, AVIF, TIFF and BMP photos to JPEG or PNG before uploading them, applying orientation and keeping transparency. The file picker, paste and drag-and-drop all use the same conversion, off the main thread.
- Upgrade the rendering stack to GPUI Kit 0.10 and GPUI Fast 0.1.17, and the embedded Chromium framework to 156.7.
- Move HTTP clients to reqwest 0.13, using the system proxy settings and trust store.

macOS build **62** bundles gateway **0.17.14** and uses protocol **104**. It can also connect to 0.17.11 and 0.17.12 gateways.
