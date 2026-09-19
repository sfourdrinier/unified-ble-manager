# scripts/ci/podspec-stub-eval.rb
#
# F01: evaluates the podspec selection logic without CocoaPods. Stubs
# Pod::Spec (attribute capture only), loads the one .podspec in --dir, and
# prints the captured attributes as JSON for
# check-podspec-rust-selection.js. Both the 5.x lane (real package.json)
# and a stubbed 4.x package.json run through this, proving the gate selects
# per lane instead of by marker presence.

require 'json'

dir = nil
ARGV.each_with_index do |arg, i|
  dir = ARGV[i + 1] if arg == '--dir'
end
abort 'usage: podspec-stub-eval.rb --dir <dir>' if dir.nil?
# Ruby glob treats backslashes as escapes even on Windows, so a native
# Windows --dir (D:\a\...) matches nothing — normalize once; forward
# slashes work everywhere in Ruby file APIs.
dir = dir.gsub('\\', '/')

module Pod
  # Mirrors CocoaPods' user-facing error class: the podspec raises it for an
  # invalid UBM_NATIVE_BUILD, and the harness must surface that message.
  class Informative < StandardError; end

  class SpecStub
    attr_reader :attrs

    def initialize
      @attrs = {}
    end

    # Pod::Specification has no getters; the podspec (like React Native's
    # install_modules_dependencies) reads state back through to_hash for
    # read-modify-write. method_missing would answer nil here and break
    # that pattern, so to_hash is explicit.
    def to_hash
      @attrs.dup
    end

    def method_missing(name, *args)
      key = name.to_s
      if key.end_with?('=')
        @attrs[key[0..-2]] = args.first
      elsif key == 'dependency'
        (@attrs['dependencies'] ||= []) << args
      else
        @attrs[key]
      end
    end

    def respond_to_missing?(*)
      true
    end
  end

  module Spec
    def self.new
      stub = SpecStub.new
      yield stub
      $captured_spec_attrs = stub.attrs
    end
  end
end

podspecs = Dir.glob(File.join(dir, '*.podspec'))
abort "expected one .podspec in #{dir}, found #{podspecs.length}" unless podspecs.length == 1
load podspecs.first
puts JSON.generate($captured_spec_attrs)
