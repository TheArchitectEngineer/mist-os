// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.light/cpp/fidl.h>
#include <fidl/fuchsia.hardware.platform.bus/cpp/driver/fidl.h>
#include <fidl/fuchsia.hardware.platform.bus/cpp/fidl.h>
#include <lib/ddk/binding.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/device.h>
#include <lib/ddk/metadata.h>
#include <lib/ddk/platform-defs.h>
#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/component/cpp/node_add_args.h>

#include <bind/fuchsia/amlogic/platform/s905d3/cpp/bind.h>
#include <bind/fuchsia/ams/platform/cpp/bind.h>
#include <bind/fuchsia/cpp/bind.h>
#include <bind/fuchsia/gpio/cpp/bind.h>
#include <bind/fuchsia/hardware/gpio/cpp/bind.h>
#include <bind/fuchsia/hardware/i2c/cpp/bind.h>
#include <bind/fuchsia/hardware/pwm/cpp/bind.h>
#include <bind/fuchsia/i2c/cpp/bind.h>
#include <bind/fuchsia/pwm/cpp/bind.h>
#include <ddktl/metadata/light-sensor.h>
#include <soc/aml-s905d2/s905d2-gpio.h>
#include <soc/aml-s905d3/s905d3-pwm.h>

#include "nelson-gpios.h"
#include "nelson.h"

namespace nelson {
namespace fpbus = fuchsia_hardware_platform_bus;

// Composite binding rules for focaltech touch driver.

zx_status_t Nelson::LightInit() {
  gpio_init_steps_.push_back(GpioPull(GPIO_RGB_SOC_INT_L, fuchsia_hardware_pin::Pull::kNone));
  gpio_init_steps_.push_back(fuchsia_hardware_pinimpl::InitStep::WithCall({{
      .pin = GPIO_RGB_SOC_INT_L,
      .call = fuchsia_hardware_pinimpl::InitCall::WithBufferMode(
          fuchsia_hardware_gpio::BufferMode::kInput),
  }}));

  metadata::LightSensorParams params = {};
  // TODO(kpt): Insert the right parameters here.
  params.integration_time_us = 711'680;
  params.gain = 64;
  params.polling_time_us = 700'000;
  const std::vector<fpbus::Metadata> kTcs3400Metadata{
      {{.id = std::to_string(DEVICE_METADATA_PRIVATE),
        .data = std::vector<uint8_t>(reinterpret_cast<uint8_t*>(&params),
                                     reinterpret_cast<uint8_t*>(&params) + sizeof(params))}},
  };

  fpbus::Node tcs3400_light_node;
  tcs3400_light_node.name() = "tcs3400_light";
  tcs3400_light_node.vid() = PDEV_VID_GENERIC;
  tcs3400_light_node.pid() = PDEV_PID_GENERIC;
  tcs3400_light_node.did() = PDEV_DID_TCS3400_LIGHT;
  tcs3400_light_node.metadata() = kTcs3400Metadata;

  const auto kI2cBindRules = std::vector{
      fdf::MakeAcceptBindRule2(bind_fuchsia_hardware_i2c::SERVICE,
                               bind_fuchsia_hardware_i2c::SERVICE_ZIRCONTRANSPORT),
      fdf::MakeAcceptBindRule2(bind_fuchsia::I2C_BUS_ID,
                               bind_fuchsia_i2c::BIND_I2C_BUS_ID_I2C_A0_0),
      fdf::MakeAcceptBindRule2(bind_fuchsia::I2C_ADDRESS,
                               bind_fuchsia_i2c::BIND_I2C_ADDRESS_AMBIENTLIGHT),
  };
  const auto kI2cProperties = std::vector{
      fdf::MakeProperty2(bind_fuchsia_hardware_i2c::SERVICE,
                         bind_fuchsia_hardware_i2c::SERVICE_ZIRCONTRANSPORT),
      fdf::MakeProperty2(bind_fuchsia::I2C_BUS_ID, bind_fuchsia_i2c::BIND_I2C_BUS_ID_I2C_A0_0),
      fdf::MakeProperty2(bind_fuchsia::I2C_ADDRESS,
                         bind_fuchsia_i2c::BIND_I2C_ADDRESS_AMBIENTLIGHT),
  };

  const auto kGpioLightInterruptRules = std::vector{
      fdf::MakeAcceptBindRule2(bind_fuchsia_hardware_gpio::SERVICE,
                               bind_fuchsia_hardware_gpio::SERVICE_ZIRCONTRANSPORT),
      fdf::MakeAcceptBindRule2(bind_fuchsia::GPIO_PIN,
                               bind_fuchsia_amlogic_platform_s905d3::GPIOAO_PIN_ID_PIN_5),
  };
  const auto kGpioLightInterruptProperties = std::vector{
      fdf::MakeProperty2(bind_fuchsia_hardware_gpio::SERVICE,
                         bind_fuchsia_hardware_gpio::SERVICE_ZIRCONTRANSPORT),
      fdf::MakeProperty2(bind_fuchsia_gpio::FUNCTION, bind_fuchsia_gpio::FUNCTION_LIGHT_INTERRUPT),
  };

  const auto kGpioInitBindRules = std::vector{
      fdf::MakeAcceptBindRule2(bind_fuchsia::INIT_STEP, bind_fuchsia_gpio::BIND_INIT_STEP_GPIO),
  };
  const auto kGpioInitProperties = std::vector{
      fdf::MakeProperty2(bind_fuchsia::INIT_STEP, bind_fuchsia_gpio::BIND_INIT_STEP_GPIO),
  };

  auto kTcs3400LightParents = std::vector{
      fuchsia_driver_framework::ParentSpec2{{
          .bind_rules = kI2cBindRules,
          .properties = kI2cProperties,
      }},
      fuchsia_driver_framework::ParentSpec2{{
          .bind_rules = kGpioLightInterruptRules,
          .properties = kGpioLightInterruptProperties,
      }},
      fuchsia_driver_framework::ParentSpec2{{
          .bind_rules = kGpioInitBindRules,
          .properties = kGpioInitProperties,
      }},
  };

  fidl::Arena<> fidl_arena;
  fdf::Arena tcs3400_light_arena('TCS3');

  auto tcs3400_light_spec = fuchsia_driver_framework::CompositeNodeSpec{
      {.name = "tcs3400_light", .parents2 = kTcs3400LightParents}};
  fdf::WireUnownedResult tsc3400_light_result =
      pbus_.buffer(tcs3400_light_arena)
          ->AddCompositeNodeSpec(fidl::ToWire(fidl_arena, tcs3400_light_node),
                                 fidl::ToWire(fidl_arena, tcs3400_light_spec));
  if (!tsc3400_light_result.ok()) {
    zxlogf(ERROR, "Failed to send AddCompositeNodeSpec request to platform bus: %s",
           tsc3400_light_result.status_string());
    return tsc3400_light_result.status();
  }
  if (tsc3400_light_result->is_error()) {
    zxlogf(ERROR, "Failed to add tcs3400_light composite node spec to platform device: %s",
           zx_status_get_string(tsc3400_light_result->error_value()));
    return tsc3400_light_result->error_value();
  }

  // Enable the Amber LED so it will be controlled by PWM.
  gpio_init_steps_.push_back(GpioFunction(GPIO_AMBER_LED_PWM, 3));  // Set as PWM.

  // GPIO must be set to default out otherwise could cause light to not work
  // on certain reboots.
  gpio_init_steps_.push_back(GpioOutput(GPIO_AMBER_LED_PWM, true));

  auto amber_led_gpio_bind_rules = std::vector{
      fdf::MakeAcceptBindRule2(bind_fuchsia_hardware_gpio::SERVICE,
                               bind_fuchsia_hardware_gpio::SERVICE_ZIRCONTRANSPORT),
      fdf::MakeAcceptBindRule2(bind_fuchsia::GPIO_PIN,
                               bind_fuchsia_amlogic_platform_s905d3::GPIOAO_PIN_ID_PIN_11),
  };

  auto amber_led_gpio_properties = std::vector{
      fdf::MakeProperty2(bind_fuchsia_hardware_gpio::SERVICE,
                         bind_fuchsia_hardware_gpio::SERVICE_ZIRCONTRANSPORT),
      fdf::MakeProperty2(bind_fuchsia_gpio::FUNCTION, bind_fuchsia_gpio::FUNCTION_GPIO_AMBER_LED),
  };

  auto amber_led_pwm_bind_rules = std::vector{
      fdf::MakeAcceptBindRule2(bind_fuchsia_hardware_pwm::SERVICE,
                               bind_fuchsia_hardware_pwm::SERVICE_ZIRCONTRANSPORT),
      fdf::MakeAcceptBindRule2(bind_fuchsia::PWM_ID,
                               bind_fuchsia_amlogic_platform_s905d3::BIND_PWM_ID_PWM_AO_A),
  };

  auto amber_led_pwm_properties = std::vector{
      fdf::MakeProperty2(bind_fuchsia_hardware_pwm::SERVICE,
                         bind_fuchsia_hardware_pwm::SERVICE_ZIRCONTRANSPORT),
      fdf::MakeProperty2(bind_fuchsia_pwm::PWM_ID_FUNCTION,
                         bind_fuchsia_pwm::PWM_ID_FUNCTION_AMBER_LED),
  };

  auto parents = std::vector{
      fuchsia_driver_framework::ParentSpec2{{
          .bind_rules = amber_led_gpio_bind_rules,
          .properties = amber_led_gpio_properties,
      }},
      fuchsia_driver_framework::ParentSpec2{{
          .bind_rules = amber_led_pwm_bind_rules,
          .properties = amber_led_pwm_properties,
      }},
      fuchsia_driver_framework::ParentSpec2{{
          .bind_rules = kGpioInitBindRules,
          .properties = kGpioInitProperties,
      }},
  };

  static const std::vector<fuchsia_hardware_light::Config> kConfigs{
      {{.name = "AMBER_LED", .brightness = true, .rgb = false, .init_on = true, .group_id = -1}}};
  static const fuchsia_hardware_light::Metadata kMetadata{{.configs = kConfigs}};

  auto metadata = fidl::Persist(kMetadata);
  if (!metadata.is_ok()) {
    zxlogf(ERROR, "Failed to persist metadata: %s",
           metadata.error_value().FormatDescription().c_str());
    return metadata.error_value().status();
  }

  fpbus::Node light_node = {};
  light_node.name() = "gpio-light";
  light_node.vid() = PDEV_VID_AMLOGIC;
  light_node.pid() = PDEV_PID_GENERIC;
  light_node.did() = PDEV_DID_GPIO_LIGHT;
  light_node.metadata() = {
      {{
          .id = fuchsia_hardware_light::Metadata::kSerializableName,
          .data = std::move(metadata.value()),
      }},
  };

  fdf::Arena arena('LIGH');
  auto aml_light_spec =
      fuchsia_driver_framework::CompositeNodeSpec{{.name = "aml_light", .parents2 = parents}};
  fdf::WireUnownedResult result = pbus_.buffer(arena)->AddCompositeNodeSpec(
      fidl::ToWire(fidl_arena, light_node), fidl::ToWire(fidl_arena, aml_light_spec));
  if (!result.ok()) {
    zxlogf(ERROR, "%s: AddCompositeNodeSpec Light(aml_light) request failed: %s", __func__,
           result.FormatDescription().data());
    return result.status();
  }
  if (result->is_error()) {
    zxlogf(ERROR, "%s: AddCompositeNodeSpec Light(aml_light) failed: %s", __func__,
           zx_status_get_string(result->error_value()));
    return result->error_value();
  }

  return ZX_OK;
}

}  // namespace nelson
