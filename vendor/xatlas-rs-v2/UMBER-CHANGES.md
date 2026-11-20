# Changes made for Umber

A copy of `xatlas-rs-v2` 0.1.4 from crates.io (MIT, wrapping xatlas by Jonathan Young, MIT).

- `Xatlas::meshes()`: a chart type outside the known values maps to `ChartType::Invalid`
  instead of panicking. xatlas leaves the type unset on charts of a UV mesh, so any repack
  (`add_uv_mesh`) panicked.

Known and worked around in Umber rather than here: `MeshData::Contiguous` counts vertices in
threes, so two-float UVs must be passed `WithStride { stride: 8 }`.
- `Xatlas::meshes()` borrows the atlas only for as long as the result is used (it was tied to
  the atlas's whole lifetime), so charts can be packed again after reading one result.
