# Generate a disposable, unhosted simulator XCTest harness around the actual
# plugin, its normal ErikaNativeLibrary loader, and NativeStub.c. Requires the
# xcodeproj gem bundled with CocoaPods; no repository Xcode project is modified.
require 'xcodeproj'
require 'fileutils'
platform, engine_dir, directory = ARGV
abort 'usage: ruby create_simulator_project.rb ios|tvos ENGINE_FRAMEWORK_DIRECTORY OUTPUT_DIRECTORY' unless %w[ios tvos].include?(platform) && engine_dir && directory
package = File.expand_path('../..', __dir__)
engine_dir = File.expand_path(engine_dir)
directory = File.expand_path(directory)
FileUtils.mkdir_p(directory)
project = Xcodeproj::Project.new("#{directory}/AppleOwner.xcodeproj")
plugin = project.new_target(:framework, 'erika_flutter', platform.to_sym, '13.0')
tests = project.new_target(:unit_test_bundle, 'RunnerTests', platform.to_sym, '13.0')
engine = project.main_group.new_file("#{engine_dir}/Flutter.framework")
Dir.glob("#{package}/#{platform}/Classes/*.swift").each do |path|
  plugin.source_build_phase.add_file_reference(project.main_group.new_file(path))
end
plugin.source_build_phase.add_file_reference(project.main_group.new_file("#{__dir__}/NativeStub.c"))
tests.source_build_phase.add_file_reference(project.main_group.new_file("#{package}/example/ios/RunnerTests/RunnerTests.swift"))
plugin.frameworks_build_phase.add_file_reference(engine)
tests.frameworks_build_phase.add_file_reference(engine)
tests.frameworks_build_phase.add_file_reference(plugin.product_reference)
tests.add_dependency(plugin)
embed = tests.new_copy_files_build_phase('Embed test frameworks')
embed.dst_subfolder_spec = '10'
[engine, plugin.product_reference].each do |ref|
  file = embed.add_file_reference(ref)
  file.settings = { 'ATTRIBUTES' => ['CodeSignOnCopy', 'RemoveHeadersOnCopy'] }
end
[plugin, tests].each do |target|
  target.build_configurations.each do |config|
    settings = config.build_settings
    settings['SWIFT_VERSION'] = '5.0'
    settings['SWIFT_ACTIVE_COMPILATION_CONDITIONS'] = 'DEBUG'
    settings['ENABLE_TESTABILITY'] = 'YES'
    settings['GENERATE_INFOPLIST_FILE'] = 'YES'
    settings['PRODUCT_BUNDLE_IDENTIFIER'] = "dev.aimesoft.appleowner.#{target.name.tr('_', '-')}"
    settings['CODE_SIGNING_ALLOWED'] = 'NO'
    settings['GCC_SYMBOLS_PRIVATE_EXTERN'] = 'NO'
    settings['HEADER_SEARCH_PATHS'] = "#{package}/native/include"
    settings['FRAMEWORK_SEARCH_PATHS'] = "#{engine_dir} $(inherited)"
    settings['LD_RUNPATH_SEARCH_PATHS'] = '$(inherited) @loader_path/Frameworks @executable_path/Frameworks'
    settings['SUPPORTED_PLATFORMS'] = platform == 'tvos' ? 'appletvsimulator' : 'iphonesimulator'
    settings['SDKROOT'] = settings['SUPPORTED_PLATFORMS']
    settings['TARGETED_DEVICE_FAMILY'] = platform == 'tvos' ? '3' : '1,2'
  end
end
project.save
scheme = Xcodeproj::XCScheme.new
scheme.add_build_target(plugin)
scheme.add_test_target(tests)
scheme.save_as(project.path, 'AppleOwner', true)
