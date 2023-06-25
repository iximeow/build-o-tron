Build.run({"cargo", "build"}, {step="build"})
Build.run({"cargo", "test"}, {step="test"})
Build.metric("test metric", 5)
