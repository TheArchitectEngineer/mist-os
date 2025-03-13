// Copyright 2025 The Fuchsia Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "sdk/lib/driver/devicetree/visitors/drivers/pci/pci.h"

#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/logging/cpp/structured_logger.h>

namespace pci_dt {

namespace {

zx::result<PciRange> ParsePciRange(devicetree::ByteView bytes, uint32_t num_size_cells,
                                   uint32_t num_parent_address_cells) {
  // First byte is the high cell of the bus address. Store it separately.
  auto bus_address_high_cell = devicetree::PropertyValue(bytes.subspan(0, sizeof(uint32_t)));

  // Parse the rest as if it was a 2-address-cell range.
  bytes = bytes.subspan(sizeof(uint32_t), bytes.size() - sizeof(uint32_t));

  return zx::ok(PciRange{
      .range = devicetree::RangesPropertyElement(
          bytes, {2 /* num_address_cells */, num_size_cells, num_parent_address_cells}),
      .bus_address_high_cell = *bus_address_high_cell.AsUint32(),
  });
}

struct InterruptParentInfo {
  uint32_t address_cells;
  uint32_t interrupt_cells;
  bool is_arm_gicv3;
};

zx::result<std::pair<uint32_t, uint32_t>> ParseAddressAndInterruptCells(
    fdf_devicetree::Node& node) {
  const auto& properties = node.properties();
  auto address_cells_prop = properties.find("#address-cells");
  if (address_cells_prop == properties.end()) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  uint32_t address_cells = *address_cells_prop->second.AsUint32();

  auto interrupt_cells_prop = properties.find("#interrupt-cells");
  if (interrupt_cells_prop == properties.end()) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  uint32_t interrupt_cells = *interrupt_cells_prop->second.AsUint32();

  return zx::ok(std::make_pair(address_cells, interrupt_cells));
}

bool InterruptControllerIsArmGicV3(fdf_devicetree::Node& interrupt_parent_node) {
  auto compatible_prop = interrupt_parent_node.properties().find("compatible");
  if (compatible_prop == interrupt_parent_node.properties().end()) {
    FDF_LOG(ERROR, "Could not find compatible node on interrupt parent");
    return false;
  }
  auto compatible_list = compatible_prop->second.AsStringList();
  if (!compatible_list) {
    FDF_LOG(ERROR, "Could not parse compatible node on interrupt parent");
    return false;
  }
  for (auto compatible : *compatible_list) {
    if (compatible == "arm,gic-v3") {
      return true;
    }
  }
  return false;
}

class InterruptParentMap {
 public:
  explicit InterruptParentMap(fdf_devicetree::Node& node) : node_(node) {}

  zx::result<InterruptParentInfo> FindInterruptParent(fdf_devicetree::Phandle phandle) {
    auto it = parent_infos_.find(phandle);
    if (it != parent_infos_.end()) {
      return zx::ok(it->second);
    }
    auto result = FindInterruptParentImpl(phandle);
    if (result.is_error()) {
      return result.take_error();
    }
    InterruptParentInfo info = *result;
    parent_infos_[phandle] = info;
    return zx::ok(info);
  }

 private:
  zx::result<InterruptParentInfo> FindInterruptParentImpl(fdf_devicetree::Phandle phandle) {
    auto ref_node = node_.GetReferenceNode(phandle);
    if (ref_node.is_error()) {
      FDF_LOG(ERROR, "Could not find interrupt parent with phandle value %u", phandle);
      return ref_node.take_error();
    }
    auto& interrupt_controller_node = *ref_node->GetNode();
    auto result = ParseAddressAndInterruptCells(interrupt_controller_node);
    if (result.is_error()) {
      return result.take_error();
    }
    auto [address_cells, interrupt_cells] = *result;
    bool is_arm_gicv3 = InterruptControllerIsArmGicV3(interrupt_controller_node);
    return zx::ok(InterruptParentInfo{
        .address_cells = address_cells,
        .interrupt_cells = interrupt_cells,
        .is_arm_gicv3 = is_arm_gicv3,
    });
  }

  std::unordered_map<fdf_devicetree::Phandle, InterruptParentInfo> parent_infos_;
  fdf_devicetree::Node& node_;
};

struct InterruptMapElementParseResult {
  std::optional<Gicv3InterruptMapElement> interrupt;
  devicetree::ByteView remaining_bytes;
};

zx::result<InterruptMapElementParseResult> ParseInterruptMapElement(
    devicetree::ByteView bytes, uint32_t address_cells, uint32_t interrupt_cells,
    InterruptParentMap& interrupts) {
  // Helper to consume a number of cells from |bytes|.
  auto advance_cells = [&bytes](uint32_t cells) -> zx::result<devicetree::ByteView> {
    size_t amount = cells * sizeof(uint32_t);
    if (bytes.size() < amount) {
      return zx::error(ZX_ERR_NOT_FOUND);
    }
    auto span = bytes.subspan(0, amount);
    bytes = bytes.subspan(amount, bytes.size() - amount);
    return zx::ok(span);
  };

  // Interrupt-map: 3-tuples of (child-interrupt, interrupt-parent, parent-interrupt)
  // child-interrupt: address (#address-cells), then interrupt (#interrupt-cells)
  auto child_address_view = advance_cells(address_cells);
  if (child_address_view.is_error()) {
    return child_address_view.take_error();
  }
  auto child_interrupt_view = advance_cells(interrupt_cells);
  if (child_interrupt_view.is_error()) {
    return child_interrupt_view.take_error();
  }

  // We interpret the child's interrupt view as the pin.
  uint32_t pin = *devicetree::PropertyValue(*child_interrupt_view).AsUint32();

  // interrupt-parent: phandle (always u32)
  auto interrupt_parent_view = advance_cells(1);
  if (interrupt_parent_view.is_error()) {
    return interrupt_parent_view.take_error();
  }
  auto interrupt_parent_phandle = *devicetree::PropertyValue(*interrupt_parent_view).AsUint32();

  // Find the interrupt parent based on phandle.
  auto interrupt_parent_info = interrupts.FindInterruptParent(interrupt_parent_phandle);
  if (interrupt_parent_info.is_error()) {
    return interrupt_parent_info.take_error();
  }

  auto parent_interrupt_address_view = advance_cells(interrupt_parent_info->address_cells);
  if (parent_interrupt_address_view.is_error()) {
    return parent_interrupt_address_view.take_error();
  }

  auto parent_interrupt_view = advance_cells(interrupt_parent_info->interrupt_cells);
  if (parent_interrupt_view.is_error()) {
    return parent_interrupt_view.take_error();
  }

  // Check that we understand the interrupt controller.
  if (!interrupt_parent_info->is_arm_gicv3) {
    FDF_LOG(ERROR, "Unsupported interrupt controller type, see https://fxbug.dev/394179809");
    // Just skip this node and return the remaining bytes so we can attempt to parse further nodes.
    return zx::ok(
        InterruptMapElementParseResult{.interrupt = std::nullopt, .remaining_bytes = bytes});
  }

  // Currently, we consider only the first cell of the child address in our address computation.
  BusAddress child_unit_address(
      *devicetree::PropertyValue(child_address_view->subspan(0, sizeof(uint32_t))).AsUint32());

  // In Gicv3, we interpret the parent interrupt specification as a 3-tuple of cells.
  auto parent_interrupt_specification =
      devicetree::PropEncodedArrayElement<3>(*parent_interrupt_view, {1, 1, 1});

  Gicv3ParentInterruptSpecifier parent(static_cast<uint32_t>(*parent_interrupt_specification[0]),
                                       static_cast<uint32_t>(*parent_interrupt_specification[1]),
                                       static_cast<uint32_t>(*parent_interrupt_specification[2]));

  Gicv3InterruptMapElement interrupt{
      .child_unit_address = child_unit_address, .pin = pin, .parent = parent};

  return zx::ok(InterruptMapElementParseResult{
      .interrupt = std::make_optional(interrupt),
      .remaining_bytes = bytes,
  });
}

zx::result<std::vector<Gicv3InterruptMapElement>> ParseInterruptMap(
    devicetree::ByteView bytes, fdf_devicetree::Node& node,
    const devicetree::PropertyDecoder& decoder) {
  std::vector<Gicv3InterruptMapElement> results;
  InterruptParentMap parent_map(node);
  while (bytes.size() > 0) {
    auto map_element = ParseInterruptMapElement(bytes, *decoder.num_address_cells(),
                                                *decoder.num_interrupt_cells(), parent_map);
    if (map_element.is_error()) {
      return map_element.take_error();
    }
    // The parser will skip elements it doesn't understand.
    if (map_element->interrupt) {
      results.push_back(std::move(*map_element->interrupt));
    }
    bytes = map_element->remaining_bytes;
  }
  return zx::ok(std::move(results));
}

}  // namespace

constexpr std::string_view kReg = "reg";
constexpr std::string_view kRanges = "ranges";
constexpr std::string_view kInterruptMap = "interrupt-map";

PciVisitor::PciVisitor() : DriverVisitor({"pci-host-cam-generic", "pci-host-ecam-generic"}) {}

zx::result<> PciVisitor::DriverVisit(fdf_devicetree::Node& node,
                                     const devicetree::PropertyDecoder& decoder) {
  auto [reg_property, ranges_property, interrupt_map_property] =
      decoder.FindProperties(kReg, kRanges, kInterruptMap);
  if (!reg_property.has_value()) {
    FDF_LOG(ERROR, "Failed to find property\"%s\".", std::string(kReg).c_str());
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  auto reg = reg_property->AsReg(decoder);
  if (!reg.has_value()) {
    FDF_LOG(ERROR, "Property is not a reg array");
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  if (reg->size() != 1u) {
    FDF_SLOG(ERROR, "Property \"reg\" expected to have one entry", KV("entries", reg->size()));
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  reg_ = (*reg)[0u];

  if (!ranges_property.has_value()) {
    FDF_LOG(ERROR, "Failed to find property\"%s\".", std::string(kRanges).c_str());
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  // We can't view this as a RangesProperty because that type assumes that addresses
  // are encoded using at most 2 cells and the PCI bus addresses use 3 cells.
  // Instead, we can skip the first cell of each range which contains the high
  // byte of the bus address and decode the rest as a regular 2-address-cell range.
  uint32_t num_address_cells = *decoder.num_address_cells();
  if (num_address_cells != 3) {
    FDF_LOG(ERROR, "Expected #address-cells to be 3 in PCI ranges property, found %u",
            num_address_cells);
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  uint32_t num_size_cells = *decoder.num_size_cells();
  uint32_t num_parent_address_cells = *decoder.parent()->num_address_cells();
  size_t range_property_length_bytes =
      (num_address_cells + num_size_cells + num_parent_address_cells) * sizeof(uint32_t);
  auto ranges_property_bytes = ranges_property->AsBytes();
  size_t num_ranges = ranges_property_bytes.size() / range_property_length_bytes;
  for (size_t i = 0; i < num_ranges; ++i) {
    size_t range_start = i * range_property_length_bytes;
    auto range_bytes = ranges_property_bytes.subspan(range_start, range_property_length_bytes);
    auto range = ParsePciRange(range_bytes, num_size_cells, num_parent_address_cells);
    if (!range.is_ok()) {
      FDF_LOG(ERROR, "Could not parse range %lu", i);
      return range.take_error();
    }
    ranges_.push_back(*range);
  }

  // interrupt-map
  if (!interrupt_map_property.has_value()) {
    FDF_LOG(ERROR, "Failed to find property\"%s\".", std::string(kInterruptMap).c_str());
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  auto interrupt_map = ParseInterruptMap(interrupt_map_property->AsBytes(), node, decoder);
  if (interrupt_map.is_error()) {
    FDF_LOG(ERROR, "Failed to parse interrupt map");
    return interrupt_map.take_error();
  }

  gic_v3_interrupt_map_elements_ = std::move(*interrupt_map);

  auto compatible = node.properties().find("compatible");
  is_extended_ = compatible->second.AsString() == std::string("pci-host-ecam-generic");

  return zx::ok();
}

}  // namespace pci_dt
