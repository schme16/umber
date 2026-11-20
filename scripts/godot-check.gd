# Loads every .tres and .glb in a Godot project's res://models and prints what Godot made of
# them: material class, which texture fills each slot, and for each model its surfaces.
#   godot --headless --path target/godot-test --import
#   godot --headless --path target/godot-test --script ../../scripts/godot-check.gd
extends SceneTree

const SLOTS = ["albedo_texture", "orm_texture", "roughness_texture", "metallic_texture", "normal_texture", "ao_texture"]

func describe(mat: Material, indent: String) -> void:
	print(indent, mat.get_class(), " \"", mat.resource_name, "\"")
	for slot in SLOTS:
		if slot in mat and mat.get(slot):
			print(indent, "  ", slot, " <- ", mat.get(slot).resource_path)
	if "normal_enabled" in mat:
		print(indent, "  normal_enabled=", mat.normal_enabled, " ao_enabled=", mat.ao_enabled)

func _init() -> void:
	for file in DirAccess.get_files_at("res://models"):
		var path = "res://models/" + file
		if file.ends_with(".tres"):
			print(path)
			describe(load(path), "  ")
		elif file.ends_with(".glb"):
			print(path)
			var scene = load(path).instantiate()
			for node in scene.find_children("*", "MeshInstance3D", true, false):
				var mesh: Mesh = node.mesh
				print("  mesh ", node.name, ": ", mesh.get_surface_count(), " surfaces, size ", mesh.get_aabb().size)
				for s in mesh.get_surface_count():
					describe(mesh.surface_get_material(s), "    ")
			scene.free()
	quit()
